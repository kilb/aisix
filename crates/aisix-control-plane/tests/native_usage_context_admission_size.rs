//! 源码感知的真实 CP HTTP/Postgres 组件回归，不称为 source-blind 或 E2E。
//! 直接提交公开观测上下文；不声称执行了网关 JWT 或 guardrail 链。

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::models::tags::{MAX_TAGS, MAX_TAG_VALUE_CHARS};
use aisix_core::models::{tag_value_from_claim, validate_apikey};
use aisix_core::native_objects::{
    configured_native_origin, native_origin_surface, platform_id_key_from_secret, NativeObjectKind,
    NativeProtocol, NativeUsageContext,
};
use aisix_core::ProviderKey;
use chrono::Utc;
use serde_json::{json, Value};
use support::world::{db_now, group_id, seed_key, seed_tenant, SHARED_GROUP};
use support::{DpClient, OperatorSession, TestCp};
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

fn snapshot(large: bool) -> Value {
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
    let context: NativeUsageContext = serde_json::from_value(json!({
        "api_key_name":"native-metadata-key","tags":tags,
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
        eprintln!("[native-context-size] stage={label}_before_complete public_tags_valid=true accepted_context_bytes={context_bytes} claimed_count=1");
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
        for event in [outbox, ingested] {
            for name in SNAPSHOT_FIELDS {
                let expected = context
                    .get(name)
                    .expect("all ten input snapshot fields are explicit");
                assert_eq!(
                    event.get(name),
                    Some(expected),
                    "{label}: complete public snapshot field {name}"
                );
            }
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
        eprintln!("[native-context-size] stage={label}_passed snapshot_fields=10 outbox_count=1 usage_count=1 charge_count=1 ledger_count=1 charge_micro_cny=124");
        charge.1.expect("the asserted nonzero charge")
    }
}

#[tokio::test]
async fn legal_public_tag_context_completes_without_truncation_or_duplicate_charge() {
    let fixture = Fixture::new().await;
    let small = fixture.run("small", snapshot(false), false).await;
    let large = fixture.run("large", snapshot(true), true).await;
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
