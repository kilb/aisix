//! 源码感知的真实 CP HTTP/Postgres 组件回归，不称为 source-blind 或 E2E。
//! 直接提交公开观测上下文；不声称执行了网关 JWT 或 guardrail 链。

mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::models::tags::{MAX_TAGS, MAX_TAG_VALUE_CHARS};
use aisix_core::models::{
    tag_value_from_claim, validate_apikey, validate_model, validate_provider_key,
};
use aisix_core::native_objects::{
    configured_native_origin, native_origin_surface, platform_id_key_from_secret, InferenceStage,
    NativeObjectKind, NativeProtocol, NativeUsageContext, TerminalUsage,
};
use aisix_core::snapshot::SnapshotHandle;
use aisix_core::{AisixSnapshot, ApiKey, Model, ProviderKey, ProxyConfig, ResourceEntry};
use aisix_gateway::Hub;
use aisix_obs::{UsageEvent, UsageSink};
use aisix_proxy::{build_router, ProxyState};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::{routing::post, Json, Router};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use support::world::{db_now, group_id, seed_key, seed_tenant, SHARED_GROUP};
use support::{DpClient, OperatorSession, TestCp};
use tower::ServiceExt;
use uuid::Uuid;

const SNAPSHOT_FIELDS: [&str; 10] = [
    "api_key_name",
    "tags",
    "jwt_subject",
    "jwt_provider",
    "jwt_claim_mapping",
    "applied_guardrails",
    "guardrail_bypassed_reason",
    "guardrail_monitor_hits",
    "guardrail_enforced_hits",
    "guardrail_scores",
];

fn uuid_at(value: &Value, pointer: &str) -> Uuid {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .unwrap_or_else(|| panic!("expected UUID at {pointer}"))
}

async fn create(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let response = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(response.status, 201, "component resource admission");
    response.body
}

// 真实公开路由产出遥测，HTTP 替身只提供官方 chat 成功形状；此 sink 不接 CP reporter。
async fn producer_event(tags: &BTreeMap<String, String>) -> UsageEvent {
    struct Upstream(Option<tokio::task::JoinHandle<()>>);
    impl Drop for Upstream {
        fn drop(&mut self) {
            if let Some(task) = &self.0 {
                task.abort();
            }
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("local component listener");
    let address = listener.local_addr().expect("local component address");
    let upstream = Router::new().route("/v1/chat/completions", post(|| async {
        Json(json!({"id":"chatcmpl_context_probe","object":"chat.completion","created":1,
            "model":"context-probe-upstream","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}))
    }));
    let mut server = Upstream(Some(tokio::spawn(async move {
        axum::serve(listener, upstream)
            .await
            .expect("local component HTTP");
    })));
    let provider_id = "11111111-1111-4111-8111-111111111111";
    let model_id = "22222222-2222-4222-8222-222222222222";
    let key_id = "33333333-3333-4333-8333-333333333333";
    let provider_doc = json!({"display_name":"context-probe-account","provider":"openai","adapter":"openai",
        "secret":"component-probe-upstream","api_base":format!("http://{address}/v1")});
    let model_doc = json!({"display_name":"context-probe","provider":"openai",
        "model_name":"context-probe-upstream","provider_key_id":provider_id});
    let key_doc = json!({"key_hash":ApiKey::hash_bearer("context-probe-caller"),
        "display_name":"native-metadata-key","allowed_model_ids":[model_id],"tags":tags});
    validate_provider_key(&provider_doc).expect("public provider resource admission");
    validate_model(&model_doc).expect("public model resource admission");
    validate_apikey(&key_doc).expect("public caller resource admission with legal tags");
    let snapshot = AisixSnapshot::new();
    snapshot.provider_keys.insert(ResourceEntry::new(
        provider_id,
        serde_json::from_value::<ProviderKey>(provider_doc).expect("typed provider"),
        1,
    ));
    snapshot.models.insert(ResourceEntry::new(
        model_id,
        serde_json::from_value::<Model>(model_doc).expect("typed model"),
        1,
    ));
    snapshot.apikeys.insert(ResourceEntry::new(
        key_id,
        serde_json::from_value::<ApiKey>(key_doc).expect("typed caller"),
        1,
    ));
    let hub = Arc::new(Hub::new());
    hub.register_specialized(
        "openai",
        Arc::new(aisix_provider_openai::OpenAiBridge::new()),
    );
    let config: ProxyConfig = serde_json::from_value(json!({"addr":"127.0.0.1:0"}))
        .expect("proxy component configuration");
    let (sender, mut events) = tokio::sync::mpsc::channel(4);
    let state = ProxyState::new(SnapshotHandle::new(snapshot), hub, &config)
        .without_cache()
        .with_usage_sink(UsageSink::new(sender));
    let request = Request::builder().method("POST").uri("/v1/chat/completions")
        .header("authorization","Bearer context-probe-caller").header("content-type","application/json")
        .extension(ConnectInfo("127.0.0.1:41000".parse::<SocketAddr>().expect("component peer")))
        .body(Body::from(json!({"model":"context-probe","messages":[{"role":"user","content":"tag producer probe"}]}).to_string()))
        .expect("public chat request");
    let response = build_router(state)
        .oneshot(request)
        .await
        .expect("actual production router");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "producer probe must execute normally"
    );
    let _ = http_body_util::Limited::new(response.into_body(), 1 << 20)
        .collect()
        .await
        .expect("drain actual chat response");
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("actual UsageSink delivery")
        .expect("one real producer event");
    assert_eq!(event.status_code, 200);
    assert_eq!((event.prompt_tokens, event.completion_tokens), (1, 1));
    assert_eq!(
        &event.tags, tags,
        "actual producer sanitize preserves all legal 128-character values"
    );
    assert_eq!(event.api_key_name, "native-metadata-key");
    if let Some(task) = server.0.take() {
        task.abort();
        let _ = task.await;
    }
    event
}

async fn snapshot(cp: &TestCp, large: bool) -> Value {
    let count = if large { MAX_TAGS } else { 1 };
    let value = if large {
        "🙂".repeat(MAX_TAG_VALUE_CHARS)
    } else {
        "small".into()
    };
    let tags: BTreeMap<String, String> = (0..count)
        .map(|index| (format!("tag{index:02}"), value.clone()))
        .collect();
    // 调用真实公开准入校验；NativeUsageContext 的宽松反序列化不能替代 tags 合法性。
    validate_apikey(&json!({"key_hash":"component-public-tag-validation","tags":tags}))
        .expect("public API-key schema admits these tags");
    for value in tags.values() {
        assert_eq!(
            tag_value_from_claim(&json!(value)).as_deref(),
            Ok(value.as_str())
        );
        assert!(value.chars().count() <= MAX_TAG_VALUE_CHARS);
    }
    assert_eq!(tags.len(), count);
    if large {
        assert_eq!(tags.values().map(String::len).sum::<usize>(), 16_384);
        assert!(
            serde_json::to_vec(&tags)
                .expect("serialize legal tags")
                .len()
                > 16_384
        );
    }
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_events")
        .fetch_one(cp.db().pool())
        .await
        .expect("usage count before isolated producer probe");
    let produced = producer_event(&tags).await;
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_events")
        .fetch_one(cp.db().pool())
        .await
        .expect("usage count after isolated producer probe");
    assert_eq!(
        after, before,
        "producer probe is captured locally and never delivered into CP"
    );
    let context: NativeUsageContext = serde_json::from_value(json!({
        "api_key_name":produced.api_key_name,"tags":produced.tags,
        "jwt_subject":"component-subject","jwt_provider":"component-provider",
        "jwt_claim_mapping":"component-claim-mapping",
        "applied_guardrails":[{"kind":"pii","hook":"input"}],
        "guardrail_bypassed_reason":"lakera_timeout",
        "guardrail_monitor_hits":[{"guardrail_name":"component-monitor","hook":"input",
            "action":"would_mask","counts":{"email":1}}],
        "guardrail_enforced_hits":[{"guardrail_name":"component-mask","hook":"input",
            "action":"masked","counts":{"email":2},"duration_us":11}],
        "guardrail_scores":[{"guardrail_name":"component-semantic","hook":"input",
            "direction":"deny","score":0.25,"threshold":0.75,"matched":false,
            "top_example_index":0,"embedding_model":"component-embedding"}]
    }))
    .expect("public typed 5+5 snapshot");
    let context = serde_json::to_value(context).expect("serialize public snapshot");
    assert_eq!(
        context.as_object().expect("context object").len(),
        SNAPSHOT_FIELDS.len()
    );
    context
}

struct Fixture {
    cp: TestCp,
    dp: DpClient,
    tenant: Uuid,
    member: Uuid,
    key: Uuid,
    model: Uuid,
    provider: Uuid,
    provider_doc: ProviderKey,
}

impl Fixture {
    fn expected_event(
        &self,
        usage: &Value,
        context: &Value,
        operation: Uuid,
        request_id: &str,
    ) -> Value {
        let usage: TerminalUsage =
            serde_json::from_value(usage.clone()).expect("public terminal usage");
        let mut event = serde_json::to_value(usage).expect("complete terminal usage serialization");
        let fields = event.as_object_mut().expect("terminal usage object");
        fields.remove("entry_id");
        fields.remove("terminal_status");
        // CP 之后才分配事件 ID；测量使用同宽合法 UUID，不把占位值作为实际身份断言。
        for (name, value) in [
            ("event_id", json!(operation)),
            ("team_id", json!(self.tenant)),
            ("api_key_id", json!(self.key)),
            ("user_id", json!(self.member)),
            ("model_id", json!(self.model)),
            ("requested_model_id", json!(self.model)),
            ("requested_service_tier", Value::Null),
            ("is_batch", json!(false)),
            ("provider_key_id", json!(self.provider)),
            ("upstream_model", json!("native-context-upstream")),
            ("upstream_provider", json!("openai")),
            ("requested_model", json!("native-context-model")),
            ("request_id", json!(request_id)),
            ("operation", json!("responses")),
            ("is_final_attempt", json!(true)),
            ("upstream_dispatched", json!(true)),
            ("inference_stage", json!(InferenceStage::Terminal)),
            ("native_operation_id", json!(operation)),
            ("native_terminal_status", json!("completed")),
        ] {
            fields.insert(name.into(), value);
        }
        for name in SNAPSHOT_FIELDS {
            fields.insert(
                name.into(),
                context
                    .get(name)
                    .expect("all ten full event fields are explicit")
                    .clone(),
            );
        }
        event
    }

    async fn new() -> Self {
        assert!(
            std::env::var("CP_TEST_DATABASE_URL").is_ok(),
            "real PG fixture is required"
        );
        let cp = TestCp::start().await.expect("real isolated CP/Postgres");
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let doc = json!({"display_name":"native-context-account","provider":"openai","adapter":"openai",
            "api_key":"sk-native-context-component-fixture","api_base":"https://native-context.example/v1",
            "groups":["shared"]});
        let provider_doc =
            serde_json::from_value(doc.clone()).expect("component provider document");
        let account = create(
            &cp,
            &admin,
            "/admin/resources/provider_keys",
            json!({"doc":doc}),
        )
        .await;
        let provider = uuid_at(&account, "/id");
        let model = create(
            &cp,
            &admin,
            "/admin/resources/models",
            json!({"doc":{
                "display_name":"native-context-model","provider":"openai",
                "model_name":"native-context-upstream","provider_key_id":provider
            }}),
        )
        .await;
        let model = uuid_at(&model, "/id");
        let tenant = seed_tenant(&cp, &admin).await;
        let tenant_path = format!("/admin/tenants/{tenant}");
        let current = cp
            .send("GET", &tenant_path, Some(admin.cookie()), None)
            .await;
        assert_eq!(current.status, 200);
        let granted = cp
            .send_conditional(
                "PATCH",
                &tenant_path,
                admin.cookie(),
                current.body["version"].as_i64().expect("tenant version"),
                json!({"allowed_model_ids":[model]}),
            )
            .await;
        assert_eq!(granted.status, 200, "actual model grant");
        let member = cp.seed_member(tenant, MemberRole::Member).await.id;
        let key = Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(member)).await)
            .expect("member key");
        let group = group_id(&cp, SHARED_GROUP).await;
        create(
            &cp,
            &admin,
            &format!("/admin/provider-groups/{group}/rates"),
            json!({
                "model_id":null,"cny_per_usd":"1","effective_from":db_now(&cp).await
            }),
        )
        .await;
        create(
            &cp,
            &admin,
            "/admin/pricing/baselines",
            json!({
                "model_id":model,"effective_from":db_now(&cp).await,
            "card":{"default":{"base":{"input":"2","output":"3","cache_read":"1"}}},
                "note":"component snapshot size does not change the token price"
            }),
        )
        .await;
        create(
            &cp,
            &admin,
            &format!("/admin/tenants/{tenant}/credits"),
            json!({
                "amount_cny":"1","reason":"组件回归测试余额"
            }),
        )
        .await;
        let balance: i64 =
            sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
                .bind(tenant)
                .fetch_one(cp.db().pool())
                .await
                .expect("real credited tenant balance");
        assert_eq!(balance, 1_000_000);
        let dp = cp.issue_dp_client("native-context-admission-size").await;
        Self {
            cp,
            dp,
            tenant,
            member,
            key,
            model,
            provider,
            provider_doc,
        }
    }

    async fn run(&self, label: &str, context: Value, large: bool) -> i64 {
        let operation = Uuid::new_v4();
        let request_id = format!("native-context-{label}");
        let boundary = configured_native_origin(
            &platform_id_key_from_secret(&self.cp.state().pki.platform_id_key_hex()),
            self.provider,
            &self.provider_doc,
            NativeProtocol::Openai,
            NativeObjectKind::Response,
        )
        .expect("source-aware component origin");
        let begun = self.dp.send("POST", "/dp/v1/native-objects/begin", Some(json!({
            "operation_id":operation,"api_key_id":self.key,"protocol":"openai","kind":"response",
            "origin":{"surface":native_origin_surface(NativeProtocol::Openai,NativeObjectKind::Response),
                "model_id":self.model,"provider_key_id":self.provider,"boundary":boundary},
            "requested_model_id":self.model,"requested_model":"native-context-model",
            "upstream_model":"native-context-upstream","upstream_provider":"openai",
            "operation":"responses","request_id":request_id,"asynchronous":true,
            "usage_context":context
        }))).await;
        assert_eq!(
            begun.status, 200,
            "{label}: legal snapshot must be accepted"
        );
        let (saved, context_bytes): (Value,i32) = sqlx::query_as(
            "SELECT submission->'usage_context',octet_length((submission->'usage_context')::text) FROM native_operations WHERE environment_id=$1 AND operation_id=$2",
        ).bind(self.cp.env().env_id).bind(operation).fetch_one(self.cp.db().pool()).await
            .expect("read exact accepted public snapshot");
        assert_eq!(
            saved, context,
            "{label}: accepted snapshot must be complete"
        );
        assert_eq!(
            context_bytes > 16_384,
            large,
            "{label}: measured PG snapshot size precondition"
        );
        let bound = self.dp.send("POST", "/dp/v1/native-objects/bind", Some(json!({
            "operation_id":operation,"api_key_id":self.key,"raw_id":format!("resp_context_{label}")
        }))).await;
        assert_eq!(bound.status, 200, "{label}: bind asynchronous response");
        assert_eq!(bound.body["record"]["completed"], false);
        let object = uuid_at(&bound.body, "/record/record_id");
        let claimed = self
            .dp
            .send(
                "POST",
                "/dp/v1/native-objects/claim",
                Some(json!({
                    "limit":1,"record_id":object,"protocol":"openai","kind":"response"
                })),
            )
            .await;
        assert_eq!(claimed.status, 200, "{label}: claim accepted response");
        let records = claimed.body["records"]
            .as_array()
            .expect("explicit claim list");
        assert_eq!(records.len(), 1, "{label}: nonempty actual claim");
        assert_eq!(records[0]["record_id"], json!(object));
        let generation = records[0]["lease_generation"]
            .as_i64()
            .expect("explicit lease generation");
        assert!(generation > 0);
        // 价格通过正式 API 生效之后取本次 UTC 时间；回放保留同一时间和非零计量。
        let occurred_at = Utc::now();
        let complete = json!({"lease":{"record_id":object,"generation":generation},
            "terminal_status":"completed","usage":[{"entry_id":"","occurred_at":occurred_at,
                "status_code":200,"prompt_tokens":37,"completion_tokens":19,
                "cached_prompt_tokens":7,"cache_usage_reported":true,"total_tokens":56}]});
        let expected_event =
            self.expected_event(&complete["usage"][0], &context, operation, &request_id);
        let full_event_bytes: i32 = sqlx::query_scalar("SELECT octet_length($1::jsonb::text)")
            .bind(expected_event)
            .fetch_one(self.cp.db().pool())
            .await
            .expect("measure the full authoritative expected event in PG");
        assert_eq!(
            full_event_bytes > 16_384,
            large,
            "{label}: complete expected event JSONB size precondition"
        );
        eprintln!("[native-context-size] stage={label}_before_complete public_tags_valid=true producer_tags_captured=true accepted_context_bytes={context_bytes} full_event_bytes={full_event_bytes} claimed_count=1");
        let completed = self
            .dp
            .send(
                "POST",
                "/dp/v1/native-objects/complete",
                Some(complete.clone()),
            )
            .await;
        assert_eq!(
            completed.status, 200,
            "{label}: legal complete must preserve all ten snapshot fields"
        );
        assert_eq!(completed.body["state"], "completed");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let delivered: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM native_terminal_outbox o JOIN usage_events u ON u.event_id=o.event_id::text WHERE o.object_id=$1 AND o.delivered_at IS NOT NULL)",
                ).bind(object).fetch_one(self.cp.db().pool()).await.expect("observe real outbox delivery");
                if delivered { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("real outbox worker must ingest the terminal event");
        let rows: Vec<(Uuid,Value,Value)> = sqlx::query_as(
            "SELECT o.event_id,o.event,u.event FROM native_terminal_outbox o JOIN usage_events u ON u.event_id=o.event_id::text WHERE o.object_id=$1",
        ).bind(object).fetch_all(self.cp.db().pool()).await.expect("read durable event snapshots");
        assert_eq!(
            rows.len(),
            1,
            "{label}: one nonempty delivered terminal event"
        );
        let (event_id, outbox, ingested) = rows.first().expect("one event");
        assert_eq!(
            outbox.get("stream"),
            context.get("stream"),
            "{label}: original stream fact belongs to the immutable canonical outbox"
        );
        assert_eq!(
            ingested.get("stream"),
            context.get("stream"),
            "{label}: terminal projection preserves true, explicit false, and legacy unknown"
        );
        for name in SNAPSHOT_FIELDS {
            let expected = context
                .get(name)
                .expect("all ten input snapshot fields are explicit");
            assert_eq!(
                ingested.get(name),
                Some(expected),
                "{label}: complete final public snapshot field {name}"
            );
            // outbox 是内部交付表示；允许从持久提交恢复观测，但已经携带的值不得改写。
            if let Some(persisted) = outbox.get(name) {
                assert_eq!(
                    persisted, expected,
                    "{label}: any outbox snapshot copy remains exact for {name}"
                );
            }
        }
        for event in [outbox, ingested] {
            for (name, expected) in [
                ("team_id", json!(self.tenant)),
                ("api_key_id", json!(self.key)),
                ("user_id", json!(self.member)),
                ("model_id", json!(self.model)),
                ("requested_model_id", json!(self.model)),
                ("provider_key_id", json!(self.provider)),
                ("occurred_at", json!(occurred_at)),
                ("request_id", json!(request_id)),
                ("native_operation_id", json!(operation)),
                ("prompt_tokens", json!(37)),
                ("completion_tokens", json!(19)),
                ("cached_prompt_tokens", json!(7)),
                ("total_tokens", json!(56)),
                ("cache_usage_reported", json!(true)),
                ("status_code", json!(200)),
            ] {
                assert_eq!(
                    event.get(name),
                    Some(&expected),
                    "{label}: authoritative event field {name}"
                );
            }
        }
        let charge: (String,Option<i64>,String,String,String) = sqlx::query_as(
            "SELECT status,charge_micro_cny,tenant_id,api_key_id,user_id FROM pricing_charges_cny WHERE event_id=$1",
        ).bind(event_id.to_string()).fetch_one(self.cp.db().pool()).await.expect("nonzero normal pricing charge");
        assert_eq!(
            charge,
            (
                "charged".into(),
                Some(124),
                self.tenant.to_string(),
                self.key.to_string(),
                self.member.to_string()
            ),
            "{label}: (37-7)*2 + 7*1 + 19*3 costs 124 micro CNY"
        );
        let replayed = self
            .dp
            .send("POST", "/dp/v1/native-objects/complete", Some(complete))
            .await;
        assert_eq!(
            replayed.status, 200,
            "{label}: same accepted complete replay"
        );
        assert_eq!(replayed.body["state"], "completed");
        let once: (i64,i64,i64,i64,i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM native_terminal_outbox WHERE object_id=$1),(SELECT count(*) FROM usage_events WHERE event_id=$2),(SELECT count(*) FROM pricing_charges_cny WHERE event_id=$2),(SELECT count(*) FROM billing_ledger_cny WHERE tenant_id=$3 AND event_id=$2 AND kind='usage'),(SELECT amount_micro_cny FROM billing_ledger_cny WHERE tenant_id=$3 AND event_id=$2 AND kind='usage')",
        ).bind(object).bind(event_id.to_string()).bind(self.tenant).fetch_one(self.cp.db().pool()).await
            .expect("one event, charge and nonzero usage ledger posting after replay");
        assert_eq!(
            once,
            (1, 1, 1, 1, -124),
            "{label}: metadata size must not alter price or once-only posting"
        );
        let saved: Value = sqlx::query_scalar("SELECT submission->'usage_context' FROM native_operations WHERE environment_id=$1 AND operation_id=$2")
            .bind(self.cp.env().env_id).bind(operation).fetch_one(self.cp.db().pool()).await
            .expect("original admission snapshot remains durable after completion and replay");
        assert_eq!(
            saved, context,
            "{label}: completion cannot discard the immutable full snapshot"
        );
        eprintln!("[native-context-size] stage={label}_passed snapshot_fields=10 outbox_count=1 usage_count=1 charge_count=1 ledger_count=1 charge_micro_cny=124");
        charge.1.expect("the asserted nonzero charge")
    }
}

#[tokio::test]
async fn legal_public_tag_context_completes_without_truncation_or_duplicate_charge() {
    let fixture = Fixture::new().await;
    let small = fixture
        .run("small", snapshot(&fixture.cp, false).await, false)
        .await;
    let large = fixture
        .run("large", snapshot(&fixture.cp, true).await, true)
        .await;
    assert_eq!(small, large, "only the legal public metadata size differs");
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(fixture.tenant)
            .fetch_one(fixture.cp.db().pool())
            .await
            .expect("real final balance");
    assert_eq!(
        balance,
        1_000_000 - small - large,
        "two completions and their replays debit only twice"
    );
}

#[tokio::test]
async fn native_stream_metadata_survives_real_cp_terminal_delivery() {
    let fixture = Fixture::new().await;
    for (label, stream) in [
        ("stream_true", Some(true)),
        ("stream_false", Some(false)),
        ("legacy_stream_unknown", None),
    ] {
        let mut context = snapshot(&fixture.cp, false).await;
        if let Some(stream) = stream {
            context["stream"] = json!(stream);
        }
        fixture.run(label, context, false).await;
    }
}
