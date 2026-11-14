// 组件合同：正式路由连本地 HTTP 上游，归属替身独立于 router 生命周期；不宣称供应商端到端。
mod raw_id_contract {
    use super::*;
    use crate::native_api::registry::{NativeObjectRegistry, NativeRegistryError};
    use aisix_core::native_objects::*;
    use futures::future::BoxFuture;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    pub(super) const MODEL: &str = "71111111-1111-4111-8111-111111111111";
    const OWNER: &str = "73333333-3333-4333-8333-333333333333";
    const FOREIGN: &str = "74444444-4444-4444-8444-444444444444";

    #[derive(Default)]
    struct Authority {
        submissions: Mutex<HashMap<uuid::Uuid, BeginRequest>>,
        objects: Mutex<HashMap<(String, String), NativeRecord>>,
        owners: Mutex<HashMap<uuid::Uuid, String>>,
        lookups: Mutex<Vec<LookupRequest>>,
        revoked: Mutex<std::collections::HashSet<uuid::Uuid>>,
    }

    macro_rules! unavailable {
        ($method:ident, $request:ty) => {
            fn $method(
                &self,
                _: $request,
            ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
                Box::pin(async { Err(NativeRegistryError::Unavailable) })
            }
        };
    }

    impl NativeObjectRegistry for Authority {
        unavailable!(owned, OwnedRequest);
        unavailable!(renew, LeaseRequest);
        unavailable!(reschedule, LeaseRequest);
        unavailable!(complete, CompleteRequest);
        unavailable!(tombstone, TombstoneRequest);

        fn claim(
            &self,
            _: ClaimRequest,
        ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async { Ok(NativeReply::new("leased")) })
        }

        fn begin(
            &self,
            request: BeginRequest,
        ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            let mut reply = NativeReply::new("pending_submission");
            reply.operation_id = Some(request.operation_id);
            reply.origin = Some(request.origin.clone());
            self.submissions
                .lock()
                .unwrap()
                .insert(request.operation_id, request);
            Box::pin(async move { Ok(reply) })
        }

        fn bind(
            &self,
            request: BindRequest,
        ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async move {
                let submission = self
                    .submissions
                    .lock()
                    .unwrap()
                    .get(&request.operation_id)
                    .cloned()
                    .ok_or(NativeRegistryError::NotFound)?;
                if request.api_key_id != submission.api_key_id {
                    return Err(NativeRegistryError::NotFound);
                }
                let mut objects = self.objects.lock().unwrap();
                let key = ("response".to_owned(), request.raw_id.clone());
                let record = objects.entry(key).or_insert_with(|| serde_json::from_value(json!({
                    "record_id":uuid::Uuid::new_v4(), "api_key_id":submission.api_key_id,
                    "operation_id":submission.operation_id,"requested_model_id":submission.requested_model_id,
                    "requested_model":submission.requested_model,"origin":submission.origin,
                    "protocol":"openai","kind":"response","raw_id":request.raw_id,"version":1
                })).unwrap()).clone();
                if record.operation_id != Some(request.operation_id)
                    || request
                        .existing_record_id
                        .is_some_and(|id| id != record.record_id)
                {
                    return Err(NativeRegistryError::Conflict);
                }
                let mut reply = NativeReply::new("bound");
                for child in request.objects {
                    let key = (child.kind.as_str().to_owned(), child.raw_id.clone());
                    let child = objects.entry(key).or_insert_with(|| serde_json::from_value(json!({
                        "record_id":uuid::Uuid::new_v4(),"api_key_id":submission.api_key_id,
                        "parent_id":record.record_id,"requested_model_id":submission.requested_model_id,
                        "requested_model":submission.requested_model,"origin":submission.origin,
                        "protocol":"openai","kind":child.kind,"raw_id":child.raw_id,"version":1
                    })).unwrap()).clone();
                    reply.records.push(child);
                }
                reply.record = Some(record);
                Ok(reply)
            })
        }

        fn lookup(
            &self,
            request: LookupRequest,
        ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
            Box::pin(async move {
                self.lookups.lock().unwrap().push(request.clone());
                if self.revoked.lock().unwrap().contains(&request.api_key_id) {
                    return Err(NativeRegistryError::NotFound);
                }
                let record = self
                    .objects
                    .lock()
                    .unwrap()
                    .get(&(
                        request.locator.kind.as_str().to_owned(),
                        request.locator.raw_id,
                    ))
                    .cloned()
                    .filter(|record| {
                        !record.deleted
                            && (record.api_key_id == Some(request.api_key_id)
                                || record.api_key_id.is_some_and(|key| {
                                    let owners = self.owners.lock().unwrap();
                                    owners.get(&key).is_some()
                                        && owners.get(&key) == owners.get(&request.api_key_id)
                                }))
                    })
                    .ok_or(NativeRegistryError::NotFound)?;
                let mut reply = NativeReply::new("found");
                reply.record = Some(record);
                Ok(reply)
            })
        }
    }

    pub(super) const OTHER_MODEL: &str = "75555555-5555-4555-8555-555555555555";

    pub(super) fn legacy_router(snapshot: AisixSnapshot, seeds: &[(&str, &str)]) -> axum::Router {
        legacy_router_with_item(snapshot, seeds, None)
    }

    pub(super) fn legacy_item_router(snapshot: AisixSnapshot) -> axum::Router {
        legacy_router_with_item(
            snapshot,
            &[("m-1", "resp_legacy_item_parent")],
            Some("msg_up_1"),
        )
    }

    fn legacy_router_with_item(
        snapshot: AisixSnapshot,
        seeds: &[(&str, &str)],
        item: Option<&str>,
    ) -> axum::Router {
        let canonical = |id: &str| match id {
            "m-1" => MODEL.to_owned(),
            "m-3" => OTHER_MODEL.to_owned(),
            "pk-gb" => "72222222-2222-4222-8222-222222222222".to_owned(),
            "k-a" => OWNER.to_owned(),
            "k-a2" => "76666666-6666-4666-8666-666666666666".to_owned(),
            "k-b" => FOREIGN.to_owned(),
            other => other.to_owned(),
        };
        let normalized = AisixSnapshot::new();
        let authority = Arc::new(Authority::default());
        for provider in snapshot.provider_keys.entries() {
            normalized.provider_keys.insert(ResourceEntry::new(
                canonical(&provider.id),
                (*provider.value).clone(),
                1,
            ));
        }
        for model in snapshot.models.entries() {
            let mut value = (*model.value).clone();
            value.provider_key_id = value.provider_key_id.map(|id| canonical(&id));
            normalized
                .models
                .insert(ResourceEntry::new(canonical(&model.id), value, 1));
        }
        for key in snapshot.apikeys.entries() {
            let id = canonical(&key.id);
            let team = if key.id == "k-b" { "foreign" } else { "owner" };
            authority
                .owners
                .lock()
                .unwrap()
                .insert(uuid::Uuid::parse_str(&id).unwrap(), team.into());
            normalized
                .apikeys
                .insert(ResourceEntry::new(id, (*key.value).clone(), 1));
        }
        let platform = crate::platform::PlatformSettings::from_secret(PLATFORM_SECRET);
        for (model, raw) in seeds {
            let model = normalized.models.get_by_id(&canonical(model)).unwrap();
            let provider = normalized
                .provider_keys
                .get_by_id(model.value.provider_key_id.as_deref().unwrap())
                .unwrap();
            let origin = OriginBinding {
                model_id: uuid::Uuid::parse_str(&model.id).unwrap(),
                provider_key_id: uuid::Uuid::parse_str(&provider.id).unwrap(),
                boundary: configured_native_origin(
                    &platform.id_key,
                    uuid::Uuid::parse_str(&provider.id).unwrap(),
                    &provider.value,
                    NativeProtocol::Openai,
                    NativeObjectKind::Response,
                )
                .unwrap(),
            };
            let operation = uuid::Uuid::new_v4();
            let record: NativeRecord = serde_json::from_value(json!({
                "record_id":uuid::Uuid::new_v4(),"operation_id":operation,"api_key_id":OWNER,
                "requested_model_id":model.id,"requested_model":model.value.display_name,
                "origin":origin,"protocol":"openai","kind":"response","raw_id":raw,"version":1
            }))
            .unwrap();
            if let Some(item) = item {
                let mut child = record.clone();
                child.parent_id = Some(record.record_id);
                child.record_id = uuid::Uuid::new_v4();
                child.kind = Some(NativeObjectKind::ResponseItem);
                child.raw_id = item.into();
                authority
                    .objects
                    .lock()
                    .unwrap()
                    .insert(("response_item".into(), item.into()), child);
            }
            authority
                .objects
                .lock()
                .unwrap()
                .insert(("response".into(), raw.to_string()), record);
        }
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        crate::build_router(
            crate::ProxyState::new(SnapshotHandle::new(normalized), hub, &cfg())
                .without_cache()
                .with_platform(platform)
                .with_native_objects(authority),
        )
    }

    fn router(base: &str, authority: Arc<Authority>) -> axum::Router {
        router_with_credential(base, authority, "sk-test")
    }

    fn router_with_credential(
        base: &str,
        authority: Arc<Authority>,
        credential: &str,
    ) -> axum::Router {
        let snapshot = new_snap_openai(base);
        let mut provider = openai_pk(base);
        Arc::make_mut(&mut provider.value).api_key = credential.into();
        snapshot.provider_keys.insert(provider);
        let model = openai_model("raw-id-model");
        snapshot
            .models
            .insert(ResourceEntry::new(MODEL, (*model.value).clone(), 1));
        snapshot.apikeys.insert(tenant_key(
            OWNER,
            "sk-raw-owner",
            json!({"team_id":"raw-owner"}),
        ));
        snapshot.apikeys.insert(tenant_key(
            FOREIGN,
            "sk-raw-foreign",
            json!({"team_id":"raw-foreign"}),
        ));
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        crate::build_router(
            crate::ProxyState::new(SnapshotHandle::new(snapshot), hub, &cfg())
                .without_cache()
                .with_platform(crate::platform::PlatformSettings::from_secret(
                    PLATFORM_SECRET,
                ))
                .with_native_objects(authority),
        )
    }

    fn addressed(verb: &str, path: &str, key: &str) -> Request<axum::body::Body> {
        Request::builder()
            .method(verb)
            .uri(path)
            .header("authorization", format!("Bearer {key}"))
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn raw_input_items_pagination_and_references_survive_a_fresh_router() {
        let upstream = MockServer::builder().start().await;
        let initial = json!({"id":"resp_raw_owned","object":"response","status":"completed","model":"private-upstream",
            "output":[{"type":"function_call","id":"fc_raw_owned","call_id":"toolu_raw_owned","name":"f","arguments":"{}"}],
            "usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}});
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(body_partial_json(json!({"input":"initial"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(initial))
            .expect(1)
            .mount(&upstream)
            .await;
        let page = json!({"object":"list","data":[{"type":"message","id":"msg_input_raw_owned","role":"user",
            "content":[{"type":"input_text","text":"earlier input"}]}],
            "first_id":"msg_input_raw_owned","last_id":"msg_input_raw_owned","has_more":true});
        Mock::given(method("GET"))
            .and(path("/v1/responses/resp_raw_owned/input_items"))
            .and(wiremock::matchers::query_param(
                "after",
                "msg_cursor_after_raw",
            ))
            .and(wiremock::matchers::query_param(
                "before",
                "msg_cursor_before_raw",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(page.clone()))
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("POST")).and(path("/v1/responses"))
            .and(body_partial_json(json!({"previous_response_id":"resp_raw_owned","input":[
                {"type":"item_reference","id":"msg_input_raw_owned"},
                {"type":"function_call","id":"fc_user_inline","call_id":"call_user_inline","name":"f","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_user_inline","output":"ok"}
            ]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"resp_raw_next","object":"response","status":"completed",
                "previous_response_id":"resp_raw_owned","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}})))
            .expect(1).mount(&upstream).await;
        let authority = Arc::new(Authority::default());
        let app = router(&upstream.uri(), authority.clone());
        let initial = app
            .clone()
            .oneshot(request_as(
                "sk-raw-owner",
                json!({"model":"raw-id-model","input":"initial"}),
            ))
            .await
            .unwrap();
        assert_eq!(initial.status(), StatusCode::OK);
        let initial: Value =
            serde_json::from_slice(&to_bytes(initial.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(initial["id"], "resp_raw_owned");
        assert_eq!(initial["output"][0]["id"], "fc_raw_owned");
        assert_eq!(initial["output"][0]["call_id"], "toolu_raw_owned");
        assert_eq!(initial["model"], "raw-id-model");
        let list=app.oneshot(addressed("GET","/v1/responses/resp_raw_owned/input_items?after=msg_cursor_after_raw&before=msg_cursor_before_raw","sk-raw-owner")).await.unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        let list: Value =
            serde_json::from_slice(&to_bytes(list.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(
            list, page,
            "raw IDs and every pagination field survive exactly"
        );
        let fresh = router(&upstream.uri(), authority.clone());
        let reference =
            |id: &str| json!({"model":"raw-id-model","input":[{"type":"item_reference","id":id}]});
        let foreign = fresh
            .clone()
            .oneshot(request_as(
                "sk-raw-foreign",
                reference("msg_input_raw_owned"),
            ))
            .await
            .unwrap();
        let unknown = fresh
            .clone()
            .oneshot(request_as("sk-raw-foreign", reference("msg_unknown")))
            .await
            .unwrap();
        assert_eq!(
            status_and_body(foreign).await,
            status_and_body(unknown).await
        );
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            2,
            "foreign and unknown references never dispatch"
        );
        let continued=fresh.oneshot(request_as("sk-raw-owner",json!({"model":"raw-id-model","previous_response_id":"resp_raw_owned","input":[
            {"type":"item_reference","id":"msg_input_raw_owned"},
            {"type":"function_call","id":"fc_user_inline","call_id":"call_user_inline","name":"f","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_user_inline","output":"ok"}
        ]}))).await.unwrap();
        assert_eq!(continued.status(), StatusCode::OK);
        let continued: Value =
            serde_json::from_slice(&to_bytes(continued.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(continued["id"], "resp_raw_next");
        assert_eq!(continued["previous_response_id"], "resp_raw_owned");
        let rotated =
            router_with_credential(&upstream.uri(), authority, "rotated-upstream-credential");
        let legacy = crate::responses_affinity::seal_response_id(
            &response_key(),
            "team:raw-owner",
            MODEL,
            "resp_raw_owned",
        );
        for id in ["resp_raw_owned", legacy.as_str()] {
            let refused = rotated
                .clone()
                .oneshot(addressed(
                    "GET",
                    &format!("/v1/responses/{id}"),
                    "sk-raw-owner",
                ))
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
            let refused = rotated
                .clone()
                .oneshot(request_as(
                    "sk-raw-owner",
                    json!({"model":"raw-id-model","previous_response_id":id,"input":"again"}),
                ))
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            3,
            "credential rotation never probes the replacement account"
        );
        upstream.verify().await;
    }

    #[tokio::test]
    async fn legacy_response_alias_without_authority_never_probes_an_account() {
        let upstream = MockServer::builder().start().await;
        let app = build_platform_app(two_tenants(&upstream.uri()));
        let own = crate::responses_affinity::seal_response_id(
            &response_key(),
            TENANT_A,
            "m-1",
            "resp_old",
        );
        for id in [own.as_str(), "resp_unknown"] {
            let response = app
                .clone()
                .oneshot(request_as(
                    "sk-caller",
                    json!({"model":"gpt-4o-resp","previous_response_id":id,"input":"continue"}),
                ))
                .await
                .unwrap();
            assert_eq!(status_and_body(response).await, platform_not_found().await);
            let response = app
                .clone()
                .oneshot(addressed(
                    "GET",
                    &format!("/v1/responses/{id}"),
                    "sk-caller",
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"]["code"], "not_found");
        }
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn known_sync_failed_response_is_relayed_once_with_raw_id_and_actual_usage() {
        let upstream = MockServer::builder().start().await;
        let failed = json!({"type":"response.failed","sequence_number":0,"response":{
            "id":"resp_failed_known","object":"response","status":"failed","model":"private-upstream","output":[],
            "error":{"code":"server_error","message":"response failed after acceptance"},
            "usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}
        }});
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("event: response.failed\ndata: {failed}\n\n"),
                "text/event-stream",
            ))
            .expect(1)
            .mount(&upstream)
            .await;
        let snapshot = new_snap_openai(&upstream.uri());
        let mut model = openai_model("accepted-failure");
        let value = Arc::make_mut(&mut model.value);
        value.stream_timeout = Some(1000);
        value.retries = Some(1);
        snapshot.models.insert(model);
        snapshot.apikeys.insert(apikey_entry(&["*"]));
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        let (tx, mut usage) = tokio::sync::mpsc::channel(8);
        let state = crate::ProxyState::new(SnapshotHandle::new(snapshot), hub, &cfg())
            .without_cache()
            .with_usage_sink(aisix_obs::UsageSink::new(tx));
        let response = crate::build_router(state)
            .oneshot(make_req(
                json!({"model":"accepted-failure","input":"hello","stream":true}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        let events = crate::redact::parse_sse_json_stream(&bytes).0;
        let failed = events
            .iter()
            .find(|event| event["type"] == "response.failed")
            .expect("the accepted failure resource is relayed");
        assert_eq!(failed["response"]["id"], "resp_failed_known");
        assert_eq!(failed["response"]["model"], "accepted-failure");
        assert_eq!(
            failed["response"]["usage"],
            json!({"input_tokens":7,"output_tokens":2,"total_tokens":9})
        );
        let row = tokio::time::timeout(std::time::Duration::from_secs(2), usage.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens, row.total_tokens),
            (7, 2, 9)
        );
        assert!(!row.usage_estimated);
        assert_eq!(row.provider_request_id, "resp_failed_known");
        assert!(row.status_code >= 400);
        assert!(
            usage.try_recv().is_err(),
            "no retry attempt or second generation was recorded"
        );
        upstream.verify().await;
    }
    #[tokio::test]
    async fn addressed_foreign_and_unknown_response_ids_use_native_404_without_dispatch() {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST")).and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"resp_saved_foreign","object":"response",
                "status":"completed","output":[],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}})))
            .expect(1).mount(&upstream).await;
        let authority = Arc::new(Authority::default());
        let app = router(&upstream.uri(), authority.clone());
        let created = app
            .clone()
            .oneshot(request_as(
                "sk-raw-foreign",
                json!({"model":"raw-id-model","input":"create"}),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let _ = to_bytes(created.into_body(), 65536).await.unwrap();
        for (verb, suffix) in [
            ("GET", ""),
            ("DELETE", ""),
            ("POST", "/cancel"),
            ("GET", "/input_items"),
        ] {
            let mut denials = Vec::new();
            for id in ["resp_saved_foreign", "resp_unknown"] {
                let response = app
                    .clone()
                    .oneshot(addressed(
                        verb,
                        &format!("/v1/responses/{id}{suffix}"),
                        "sk-raw-owner",
                    ))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{verb} {suffix}");
                let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(body["error"]["code"], "not_found");
                denials.push(body);
            }
            assert_eq!(
                denials[0], denials[1],
                "foreign and missing addressed resources are indistinguishable"
            );
        }
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            1,
            "addressed misses never probe an upstream account"
        );
        assert_eq!(
            authority.submissions.lock().unwrap().len(),
            1,
            "management cannot create another inference"
        );
        upstream.verify().await;
    }
    fn legacy_reference_router(
        base: &str,
        authority: Arc<Authority>,
        allowed: &[&str],
        credential: &str,
    ) -> axum::Router {
        let snapshot = new_snap_openai(base);
        let mut provider = openai_pk(base);
        Arc::make_mut(&mut provider.value).api_key = credential.into();
        snapshot.provider_keys.insert(provider);
        for (id, name) in [(MODEL, "legacy-old"), (OTHER_MODEL, "legacy-new")] {
            let model = openai_model(name);
            snapshot
                .models
                .insert(ResourceEntry::new(id, (*model.value).clone(), 1));
        }
        for (id, key) in [(OWNER, "sk-legacy-owner"), (FOREIGN, "sk-legacy-peer")] {
            snapshot.apikeys.insert(tenant_key(
                id,
                key,
                json!({"team_id":"legacy-shared-tenant","allowed_models":allowed}),
            ));
        }
        let hub = Arc::new(Hub::new());
        hub.register_specialized("openai", Arc::new(OpenAiBridge::new()));
        crate::build_router(
            crate::ProxyState::new(SnapshotHandle::new(snapshot), hub, &cfg())
                .without_cache()
                .with_platform(crate::platform::PlatformSettings::from_secret(
                    PLATFORM_SECRET,
                ))
                .with_native_objects(authority),
        )
    }

    fn legacy_reference_request(slot: &str, id: &str) -> Value {
        match slot {
            "item" => json!({"model":"legacy-new","input":[{"type":"item_reference","id":id}]}),
            "container_tool" => {
                json!({"model":"legacy-new","input":"continue","tools":[{"type":"code_interpreter","container":id}]})
            }
            "container_input" => {
                json!({"model":"legacy-new","input":[{"type":"code_interpreter_call","id":"ci_inline","container_id":id,"code":"1","outputs":[],"status":"completed"}]})
            }
            "container_environment" => {
                json!({"model":"legacy-new","input":"continue","tools":[{"type":"shell","environment":{"type":"container_reference","container_id":id}}]})
            }
            _ => panic!("unknown reference slot"),
        }
    }

    async fn legacy_reference_authority_contract(slots: &[&str]) {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST")).and(path("/v1/responses")).and(body_partial_json(json!({"input":"seed"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"resp_legacy_seed","object":"response","status":"completed",
                "output":[{"type":"message","id":"msg_legacy_owned","role":"assistant","status":"completed","content":[{"type":"output_text","text":"previous","annotations":[]}]},
                    {"type":"code_interpreter_call","id":"ci_legacy_owned","container_id":"cntr_legacy_owned","code":"1","outputs":[],"status":"completed"}],
                "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}})))
            .with_priority(1).expect(1).mount(&upstream).await;
        let sequence = std::sync::atomic::AtomicUsize::new(0);
        Mock::given(method("POST")).and(path("/v1/responses"))
            .respond_with(move |_:&wiremock::Request|ResponseTemplate::new(200).set_body_json(json!({
                "id":format!("resp_legacy_follow_{}",sequence.fetch_add(1,std::sync::atomic::Ordering::SeqCst)),
                "object":"response","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}})))
            .expect(slots.len() as u64+1).mount(&upstream).await;
        let authority = Arc::new(Authority::default());
        let allowed = ["legacy-old", "legacy-new"];
        let app = legacy_reference_router(&upstream.uri(), authority.clone(), &allowed, "sk-test");
        let seeded = app
            .clone()
            .oneshot(request_as(
                "sk-legacy-owner",
                json!({"model":"legacy-old","input":"seed"}),
            ))
            .await
            .unwrap();
        assert_eq!(seeded.status(), StatusCode::OK);
        let _ = to_bytes(seeded.into_body(), 65536).await.unwrap();
        let platform = crate::platform::PlatformSettings::from_secret(PLATFORM_SECRET);
        let sealer = crate::opaque_id::Sealer::of(&platform, "team:legacy-shared-tenant");
        for slot in slots {
            let (kind, raw) = if *slot == "item" {
                (crate::opaque_id::IdKind::Item, "msg_legacy_owned")
            } else {
                (crate::opaque_id::IdKind::Container, "cntr_legacy_owned")
            };
            let alias = sealer.seal(kind, raw);
            let lookups = authority.lookups.lock().unwrap().len();
            let accepted = app
                .clone()
                .oneshot(request_as(
                    "sk-legacy-owner",
                    legacy_reference_request(slot, &alias),
                ))
                .await
                .unwrap();
            assert_eq!(
                accepted.status(),
                StatusCode::OK,
                "a current owned alias remains usable"
            );
            let _ = to_bytes(accepted.into_body(), 65536).await.unwrap();
            assert_eq!(
                authority.lookups.lock().unwrap().len(),
                lookups + 1,
                "legacy stored aliases require the same current authority query as raw IDs"
            );
            let requests = upstream.received_requests().await.unwrap();
            let sent: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
            let pointer = match *slot {
                "item" => "/input/0/id",
                "container_tool" => "/tools/0/container",
                "container_input" => "/input/0/container_id",
                _ => "/tools/0/environment/container_id",
            };
            assert_eq!(sent.pointer(pointer).and_then(Value::as_str), Some(raw));
            for scenario in [
                "peer",
                "model_revoked",
                "authority_revoked",
                "account_rotated",
                "unregistered",
            ] {
                let chosen = if scenario == "unregistered" {
                    Arc::new(Authority::default())
                } else {
                    authority.clone()
                };
                if scenario == "authority_revoked" {
                    authority
                        .revoked
                        .lock()
                        .unwrap()
                        .insert(uuid::Uuid::parse_str(OWNER).unwrap());
                }
                let grants = if matches!(scenario, "peer" | "model_revoked") {
                    vec!["legacy-new"]
                } else {
                    allowed.to_vec()
                };
                let credential = if scenario == "account_rotated" {
                    "rotated-credential"
                } else {
                    "sk-test"
                };
                let rejecting =
                    legacy_reference_router(&upstream.uri(), chosen, &grants, credential);
                let caller = if scenario == "peer" {
                    "sk-legacy-peer"
                } else {
                    "sk-legacy-owner"
                };
                let count = upstream.received_requests().await.unwrap().len();
                let raw_response = rejecting
                    .clone()
                    .oneshot(request_as(caller, legacy_reference_request(slot, raw)))
                    .await
                    .unwrap();
                let raw_denial = status_and_body(raw_response).await;
                assert!(
                    !raw_denial.0.is_success(),
                    "the scenario must deny the raw stored reference"
                );
                let aliased_response = rejecting
                    .oneshot(request_as(caller, legacy_reference_request(slot, &alias)))
                    .await
                    .unwrap();
                assert_eq!(status_and_body(aliased_response).await,raw_denial,"{slot} {scenario}: a tenant seal cannot replace current ownership/model/account authorization");
                assert_eq!(
                    upstream.received_requests().await.unwrap().len(),
                    count,
                    "denied raw and aliased references never probe an account"
                );
                authority.revoked.lock().unwrap().clear();
            }
        }
        // 完整内联消息、调用和结果只恢复协议 ID，不要求任何历史对象预登记。
        let empty = Arc::new(Authority::default());
        let inline =
            legacy_reference_router(&upstream.uri(), empty.clone(), &["legacy-new"], "sk-test");
        let call = sealer.seal(crate::opaque_id::IdKind::Call, "call_inline_only");
        let accepted=inline.oneshot(request_as("sk-legacy-owner",json!({"model":"legacy-new","input":[
            {"type":"message","id":sealer.seal(crate::opaque_id::IdKind::Item,"msg_inline_only"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"inline","annotations":[]}]},
            {"type":"function_call","id":sealer.seal(crate::opaque_id::IdKind::Item,"fc_inline_only"),"call_id":call,"name":"f","arguments":"{}"},
            {"type":"function_call_output","call_id":call,"output":"ok"}
        ]}))).await.unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let _ = to_bytes(accepted.into_body(), 65536).await.unwrap();
        assert!(
            empty.lookups.lock().unwrap().is_empty(),
            "inline histories do not fetch stored objects"
        );
        let sent = upstream.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&sent.last().unwrap().body).unwrap();
        assert_eq!(sent["input"][0]["id"], "msg_inline_only");
        assert_eq!(sent["input"][1]["id"], "fc_inline_only");
        assert_eq!(sent["input"][1]["call_id"], "call_inline_only");
        assert_eq!(sent["input"][2]["call_id"], "call_inline_only");
        upstream.verify().await;
    }

    #[tokio::test]
    async fn legacy_stored_item_alias_requires_current_durable_authority() {
        legacy_reference_authority_contract(&["item"]).await;
    }

    #[tokio::test]
    async fn legacy_stored_container_alias_requires_current_durable_authority() {
        legacy_reference_authority_contract(&[
            "container_tool",
            "container_input",
            "container_environment",
        ])
        .await;
    }
}
