//! 真实 CP/Postgres 组件计数；不是 HTTP E2E，不替代已发布的 gateway 旅程。
//! 计数仅包住真实 begin 事务的资源加载、属性 SELECT 和工作副本执行点。
use super::*;
use crate::auth::{MemberRole, OperatorRole};
use std::cell::Cell;

#[path = "../../../tests/support/mod.rs"]
mod support;
use support::world::{seed_key, seed_tenant};
use support::{OperatorSession, TestCp};

#[derive(Clone, Copy, Debug, Default)]
struct Cost {
    resource_loads: usize,
    scalar_reads: usize,
    working_copies: usize,
    copied_entry_cells: usize,
    copied_root_contexts: usize,
}

tokio::task_local! {
    static COST: Cell<Cost>;
}

pub(super) fn resource_load() {
    let _ = COST.try_with(|slot| {
        let mut cost = slot.get();
        cost.resource_loads += 1;
        slot.set(cost);
    });
}

pub(super) fn scalar_read() {
    let _ = COST.try_with(|slot| {
        let mut cost = slot.get();
        cost.scalar_reads += 1;
        slot.set(cost);
    });
}

pub(super) fn working_copy(body: &BeginRequest) {
    let _ = COST.try_with(|slot| {
        let mut cost = slot.get();
        cost.working_copies += 1;
        cost.copied_entry_cells += body.entries.len();
        cost.copied_root_contexts += usize::from(body.usage_context.is_some());
        slot.set(cost);
    });
}

const BASE: &str = "https://native-batch-preload.example/v1";
const SECRET: &str = "native-batch-preload-component-credential";

fn uuid_at(value: &Value, pointer: &str) -> Uuid {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(|| panic!("UUID at {pointer}: {value}"))
}

async fn create(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let response = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(response.status, 201, "real resource setup: {response:?}");
    response.body
}

struct Fixture {
    cp: TestCp,
    dp: DpIdentity,
    tenant: Uuid,
    key: Uuid,
    provider: Uuid,
    model_a: Uuid,
    model_b: Uuid,
    denied_model: Uuid,
    boundary: OriginFingerprint,
}

impl Fixture {
    async fn new() -> Self {
        let cp = TestCp::start()
            .await
            .expect("CP_TEST_DATABASE_URL is required for real PG");
        let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
        let provider_doc = json!({"display_name":"batch-preload-account", "provider":"openai",
            "api_key":SECRET, "api_base":BASE, "groups":["shared"]});
        let provider = uuid_at(
            &create(
                &cp,
                &admin,
                "/admin/resources/provider_keys",
                json!({"doc":provider_doc.clone()}),
            )
            .await,
            "/id",
        );
        let mut models = Vec::new();
        for (name, upstream) in [
            ("batch-a/*", "upstream-a"),
            ("batch-b/*", "upstream-b"),
            ("batch-denied/*", "upstream-denied"),
        ] {
            let model = create(
                &cp,
                &admin,
                "/admin/resources/models",
                json!({"doc":{
                    "display_name":name, "provider":"openai", "model_name":upstream,
                    "provider_key_id":provider
                }}),
            )
            .await;
            models.push(uuid_at(&model, "/id"));
        }
        let tenant = seed_tenant(&cp, &admin).await;
        let path = format!("/admin/tenants/{tenant}");
        let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
        assert_eq!(current.status, 200, "real tenant setup");
        let granted = cp
            .send_conditional(
                "PATCH",
                &path,
                admin.cookie(),
                current.body["version"].as_i64().unwrap(),
                json!({"allowed_model_ids":[models[0],models[1]]}),
            )
            .await;
        assert_eq!(
            granted.status, 200,
            "grant both allowed wildcard rows: {granted:?}"
        );
        let member = cp.seed_member(tenant, MemberRole::Member).await.id;
        let key = Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(member)).await).unwrap();
        let gateway_admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let bundle = cp
            .issue_bundle(&gateway_admin, "native-batch-preload-gateway")
            .await;
        let dp = DpIdentity {
            dp_id: Uuid::parse_str(&bundle.dp_id).unwrap(),
        };
        let provider_value: ProviderKey = serde_json::from_value(provider_doc).unwrap();
        let boundary = configured_native_origin(
            &platform_id_key_from_secret(&cp.state().pki.platform_id_key_hex()),
            provider,
            &provider_value,
            NativeProtocol::Openai,
            NativeObjectKind::MessageBatch,
        )
        .unwrap();
        Self {
            cp,
            dp,
            tenant,
            key,
            provider,
            model_a: models[0],
            model_b: models[1],
            denied_model: models[2],
            boundary,
        }
    }

    fn body(&self, count: usize) -> BeginRequest {
        let entries: Vec<Value> = (0..count).map(|index| json!({
            "entry_id":format!("entry-{index:04}"), "endpoint":"/v1/chat/completions",
            "model_id":self.model_a, "requested_model_id":self.model_a,
            "requested_model":"batch-a/root", "upstream_model":"upstream-a", "upstream_provider":"openai",
            "usage_context":{"applied_guardrails":[{"kind":"pii","hook":"input"}]}
        })).collect();
        serde_json::from_value(json!({
            "operation_id":Uuid::new_v4(), "api_key_id":self.key,
            "protocol":"openai", "kind":"message_batch",
            "origin":{"model_id":self.model_a,"provider_key_id":self.provider,"boundary":self.boundary},
            "requested_model_id":self.model_a,"requested_model":"batch-a/root",
            "upstream_model":"upstream-a","upstream_provider":"openai",
            "operation":"batches", "request_id":"native-batch-preload-component", "asynchronous":true,
            "entries":entries, "usage_context":{"api_key_name":"preserved-root-context","jwt_subject":"preserved-root-subject"}
        })).unwrap()
    }

    async fn invoke(&self, body: BeginRequest) -> (Result<NativeReply, ApiError>, Cost) {
        let mut tx = self.cp.state().native_objects.pool().begin().await.unwrap();
        let (result, cost) = COST
            .scope(Cell::new(Cost::default()), async {
                let result =
                    begin_with_locked_parent(&mut tx, self.cp.state(), self.dp, body, None).await;
                (result, COST.with(Cell::get))
            })
            .await;
        if result.is_ok() {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
        (result, cost)
    }

    async fn saved(&self, operation: Uuid) -> Value {
        let (tenant, key, dp, saved): (Uuid, Uuid, Uuid, Value) = sqlx::query_as("SELECT tenant_id,api_key_id,dp_id,submission FROM native_operations WHERE environment_id=$1 AND operation_id=$2")
            .bind(self.cp.env().env_id).bind(operation).fetch_one(self.cp.db().pool()).await.unwrap();
        assert_eq!((tenant, key, dp), (self.tenant, self.key, self.dp.dp_id));
        saved
    }

    async fn assert_not_accepted(&self, operation: Uuid) {
        let copies: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM native_operations WHERE environment_id=$1 AND operation_id=$2",
        )
        .bind(self.cp.env().env_id)
        .bind(operation)
        .fetch_one(self.cp.db().pool())
        .await
        .unwrap();
        assert_eq!(copies, 0, "拒绝的 entry 不能留下 accepted 事实");
    }
}

async fn bounded_cost(count: usize) {
    let fixture = Fixture::new().await;
    let body = fixture.body(count);
    let operation = body.operation_id;
    let expected = serde_json::to_value(&body).unwrap();
    let (result, cost) = fixture.invoke(body).await;
    let reply = result.expect("valid same-account batch must be admitted before measuring");
    assert_eq!(reply.state, "pending_submission");
    assert_eq!(
        fixture.saved(operation).await,
        expected,
        "优化不得清掉持久原条目或观测上下文"
    );
    eprintln!("native_begin_component count={count} cost={cost:?}");
    // 当前81a已把深复制清单压到一次；不能把已修复的N²当成本轮RED。
    assert!(cost.copied_entry_cells <= count);
    assert!(cost.copied_root_contexts <= 1);
    assert_eq!(cost.resource_loads, 1, "同一事务只预载一次完整授权资源");
    assert_eq!(cost.scalar_reads, 0, "已加载模型的三个字段不能再逐项SELECT");
    assert!(
        cost.working_copies <= 1,
        "entry工作副本应复用，不能每项clone"
    );
}

#[tokio::test]
async fn native_begin_preloads_resources_once_for_32_entries() {
    bounded_cost(32).await;
}

#[tokio::test]
async fn native_begin_preloads_resources_once_for_1000_entries() {
    bounded_cost(1000).await;
}

#[tokio::test]
async fn native_begin_checks_the_actual_b_entry_alias_after_a_root() {
    let fixture = Fixture::new().await;
    let mut body = fixture.body(2);
    let entry = &mut body.entries[1];
    entry.model_id = fixture.model_b;
    entry.requested_model_id = fixture.model_b;
    entry.requested_model = "batch-b/allowed".into();
    entry.upstream_model = "upstream-b".into();
    let operation = body.operation_id;
    let expected = serde_json::to_value(&body).unwrap();
    let (result, _) = fixture.invoke(body).await;
    assert_eq!(
        result
            .expect("A root and granted B entry alias must each authorize")
            .state,
        "pending_submission"
    );
    assert_eq!(fixture.saved(operation).await, expected);
}

#[tokio::test]
async fn native_begin_refuses_an_entry_alias_outside_its_wildcard_despite_valid_root() {
    let fixture = Fixture::new().await;
    let mut body = fixture.body(2);
    body.entries[1].requested_model = "outside-the-granted-wildcard/forbidden".into();
    let operation = body.operation_id;
    let (result, _) = fixture.invoke(body).await;
    assert_eq!(
        result
            .expect_err("root alias cannot authorize an invalid entry alias")
            .status,
        axum::http::StatusCode::NOT_FOUND
    );
    fixture.assert_not_accepted(operation).await;
}

#[tokio::test]
async fn native_begin_refuses_an_ungranted_entry_model_even_after_valid_entries() {
    let fixture = Fixture::new().await;
    let mut body = fixture.body(32);
    let entry = &mut body.entries[31];
    entry.model_id = fixture.denied_model;
    entry.requested_model_id = fixture.denied_model;
    entry.requested_model = "batch-denied/forbidden".into();
    entry.upstream_model = "upstream-denied".into();
    let operation = body.operation_id;
    let (result, _) = fixture.invoke(body).await;
    assert_eq!(
        result.expect_err("预载到资源不代表调用方有权使用").status,
        axum::http::StatusCode::NOT_FOUND
    );
    fixture.assert_not_accepted(operation).await;
}
