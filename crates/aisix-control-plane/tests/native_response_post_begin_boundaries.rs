//! 源码感知的真实 CP HTTP/Postgres 组件回归，不称为 source-blind 或 E2E。
//! 只验证已接受操作的归属、执行模板与产物持久事实，不验证计费流水或非零用量。

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::native_objects::{
    configured_native_origin, native_origin_surface, platform_id_key_from_secret, NativeObjectKind,
    NativeProtocol,
};
use aisix_core::ProviderKey;
use serde_json::{json, Value};
use support::world::{seed_key, seed_tenant};
use support::{DpClient, OperatorSession, TestCp};
use uuid::Uuid;

const BEGIN: &str = "/dp/v1/native-objects/begin";
const BIND: &str = "/dp/v1/native-objects/bind";

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
    assert_eq!(response.status, 201, "component resource creation");
    response.body
}

struct Fixture {
    cp: TestCp,
    dp: DpClient,
    keys: [Uuid; 2],
    members: [Uuid; 2],
    models: [Uuid; 2],
    provider: Uuid,
    provider_doc: ProviderKey,
}

struct Accepted {
    conversation: Uuid,
    version: i64,
    epoch: i64,
    operation: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        assert!(std::env::var("CP_TEST_DATABASE_URL").is_ok());
        let cp = TestCp::start().await.expect("real isolated CP/Postgres");
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let doc = json!({"display_name":"post-begin-account","provider":"openai","adapter":"openai",
            "api_key":"sk-post-begin-component-fixture","api_base":"https://post-begin.example/v1",
            "groups":["shared"]});
        let provider_doc =
            serde_json::from_value(doc.clone()).expect("component provider document");
        let provider = create(
            &cp,
            &admin,
            "/admin/resources/provider_keys",
            json!({"doc":doc}),
        )
        .await;
        let provider = uuid_at(&provider, "/id");
        let mut model_ids = Vec::new();
        for suffix in ["a", "b"] {
            let model = create(
                &cp,
                &admin,
                "/admin/resources/models",
                json!({"doc":{
                    "display_name":format!("post-begin-model-{suffix}"),"provider":"openai",
                    "model_name":format!("post-begin-upstream-{suffix}"),"provider_key_id":provider
                }}),
            )
            .await;
            model_ids.push(uuid_at(&model, "/id"));
        }
        let models = [model_ids[0], model_ids[1]];
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
                json!({"allowed_model_ids":models}),
            )
            .await;
        assert_eq!(granted.status, 200, "both actual models are granted");
        // 复用既有组件 fixture 的正式成员创建函数，不直接写角色或对象表。
        let a = cp.seed_member(tenant, MemberRole::Member).await.id;
        let b = cp.seed_member(tenant, MemberRole::Admin).await.id;
        let key_a = Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(a)).await).expect("A key");
        let key_b = Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(b)).await).expect("B key");
        let dp = cp.issue_dp_client("post-begin-boundaries").await;
        Self {
            cp,
            dp,
            keys: [key_a, key_b],
            members: [a, b],
            models,
            provider,
            provider_doc,
        }
    }

    async fn begin(&self, kind: NativeObjectKind, actor: usize, parent: Option<Uuid>) -> Uuid {
        let operation = Uuid::new_v4();
        let boundary = configured_native_origin(
            &platform_id_key_from_secret(&self.cp.state().pki.platform_id_key_hex()),
            self.provider,
            &self.provider_doc,
            NativeProtocol::Openai,
            kind,
        )
        .expect("source-aware component origin");
        let suffix = ["a", "b"][actor];
        let response = self.dp.send("POST", BEGIN, Some(json!({
            "operation_id":operation,"api_key_id":self.keys[actor],"protocol":"openai","kind":kind,
            "origin":{"surface":native_origin_surface(NativeProtocol::Openai,kind),
                "model_id":self.models[actor],"provider_key_id":self.provider,"boundary":boundary},
            "parent_id":parent,"requested_model_id":self.models[actor],
            "requested_model":format!("post-begin-model-{suffix}"),
            "upstream_model":format!("post-begin-upstream-{suffix}"),"upstream_provider":"openai",
            "operation":if kind==NativeObjectKind::Conversation {"conversations"} else {"responses"},
            "request_id":format!("post-begin-{operation}"),"asynchronous":false
        }))).await;
        assert_eq!(
            response.status, 200,
            "legal accepted begin before C changes"
        );
        operation
    }

    async fn accepted_response(&self) -> Accepted {
        let c_operation = self.begin(NativeObjectKind::Conversation, 0, None).await;
        let c = self
            .dp
            .send(
                "POST",
                BIND,
                Some(json!({
                    "operation_id":c_operation,"api_key_id":self.keys[0],"raw_id":"conv_post_begin"
                })),
            )
            .await;
        assert_eq!(c.status, 200, "A creates the conversation");
        assert_eq!(c.body["record"]["api_key_id"], json!(self.keys[0]));
        let conversation = uuid_at(&c.body, "/record/record_id");
        let version = c.body["record"]["version"].as_i64().expect("C version");
        let (stored_version, epoch): (i64, i64) = sqlx::query_as(
            "SELECT version,membership_epoch FROM native_objects WHERE environment_id=$1 AND id=$2",
        )
        .bind(self.cp.env().env_id)
        .bind(conversation)
        .fetch_one(self.cp.db().pool())
        .await
        .expect("read C version and internal epoch before accepting R");
        assert_eq!(stored_version, version);
        let operation = self
            .begin(NativeObjectKind::Response, 1, Some(conversation))
            .await;
        let accepted: (Uuid,Uuid,Option<Uuid>,Uuid,i64,bool) = sqlx::query_as(
            "SELECT api_key_id,member_id,(submission->>'parent_id')::uuid,(submission->'origin'->>'model_id')::uuid,(SELECT count(*) FROM native_objects o WHERE o.environment_id=op.environment_id AND o.operation_id=op.operation_id),visibility_full FROM native_operations op WHERE environment_id=$1 AND operation_id=$2",
        ).bind(self.cp.env().env_id).bind(operation).fetch_one(self.cp.db().pool()).await
            .expect("read accepted submission before bind");
        assert_eq!(
            accepted,
            (
                self.keys[1],
                self.members[1],
                Some(conversation),
                self.models[1],
                0,
                true
            ),
            "R is accepted as B with exact C before any root/output object exists"
        );
        Accepted {
            conversation,
            version,
            epoch,
            operation,
        }
    }

    async fn bind_response(&self, accepted: &Accepted) -> Value {
        let bound = self.dp.send("POST", BIND, Some(json!({
            "operation_id":accepted.operation,"api_key_id":self.keys[1],"raw_id":"resp_post_begin",
            "objects":[{"kind":"response_item","raw_id":"msg_post_begin"}]
        }))).await;
        assert_eq!(
            bound.status, 200,
            "C lifecycle changes cannot erase the accepted R/I result"
        );
        assert_eq!(
            bound.body["record"]["operation_id"],
            json!(accepted.operation)
        );
        assert_eq!(bound.body["record"]["api_key_id"], json!(self.keys[1]));
        assert_eq!(
            bound.body["record"]["origin"]["model_id"],
            json!(self.models[1])
        );
        assert_eq!(bound.body["records"][0]["api_key_id"], json!(self.keys[0]));
        assert_eq!(
            bound.body["records"][0]["origin"]["model_id"],
            json!(self.models[1])
        );
        bound.body
    }

    async fn assert_durable_result(&self, accepted: &Accepted, bound: &Value) {
        let root = uuid_at(bound, "/record/record_id");
        let item = uuid_at(bound, "/records/0/record_id");
        let row: DurableResult = sqlx::query_as(
            "SELECT r.api_key_id AS root_owner,r.member_id AS root_member,r.model_id AS root_model,r.parent_id AS root_parent,i.api_key_id AS item_owner,i.member_id AS item_member,i.model_id AS item_model,i.parent_id AS item_parent,rop.api_key_id AS root_actor,iop.api_key_id AS item_actor,iop.member_id AS item_actor_member,(iop.submission->'origin'->>'model_id')::uuid AS item_execution_model,EXISTS(SELECT 1 FROM native_operation_origin_sources s WHERE s.environment_id=i.environment_id AND s.operation_id=i.operation_id AND s.source_record_id=r.id) AS source_is_root,(SELECT count(*) FROM native_object_memberships m WHERE m.environment_id=r.environment_id AND m.parent_id=r.id AND m.child_id=i.id AND m.relation='response_output' AND m.deleted_at IS NULL) AS root_outputs,(SELECT count(*) FROM native_object_memberships m WHERE m.environment_id=r.environment_id AND m.parent_id=$4 AND m.child_id=i.id AND m.relation='conversation_item') AS conversation_memberships FROM native_objects r JOIN native_objects i ON i.environment_id=r.environment_id AND i.id=$3 JOIN native_operations rop ON rop.environment_id=r.environment_id AND rop.operation_id=r.operation_id JOIN native_operations iop ON iop.environment_id=i.environment_id AND iop.operation_id=i.operation_id WHERE r.environment_id=$1 AND r.id=$2",
        ).bind(self.cp.env().env_id).bind(root).bind(item).bind(accepted.conversation)
            .fetch_one(self.cp.db().pool()).await.expect("read persisted R/I ownership and execution facts");
        assert_eq!(row.root_owner, self.keys[1]);
        assert_eq!(row.root_member, Some(self.members[1]));
        assert_eq!(row.root_model, self.models[1]);
        assert_eq!(row.root_parent, Some(accepted.conversation));
        assert_eq!(row.item_owner, self.keys[0]);
        assert_eq!(row.item_member, Some(self.members[0]));
        assert_eq!(row.item_model, self.models[1]);
        assert_eq!(row.item_parent, Some(root));
        assert_eq!(row.root_actor, self.keys[1]);
        assert_eq!(row.item_actor, self.keys[1]);
        assert_eq!(row.item_actor_member, Some(self.members[1]));
        assert_eq!(row.item_execution_model, self.models[1]);
        assert!(row.source_is_root, "I execution source is accepted R");
        assert_eq!(row.root_outputs, 1, "formal R output remains durably bound");
        assert_eq!(
            row.conversation_memberships, 0,
            "bind cannot claim the changed C membership"
        );
        let lookup = self.dp.send("POST", "/dp/v1/native-objects/lookup", Some(json!({
            "api_key_id":self.keys[1],"locator":{"protocol":"openai","kind":"response","raw_id":"resp_post_begin"}
        }))).await;
        assert_eq!(
            lookup.status, 200,
            "B can still read its own accepted response"
        );
        assert_eq!(lookup.body["record"]["record_id"], json!(root));
    }
}

#[derive(sqlx::FromRow)]
struct DurableResult {
    root_owner: Uuid,
    root_member: Option<Uuid>,
    root_model: Uuid,
    root_parent: Option<Uuid>,
    item_owner: Uuid,
    item_member: Option<Uuid>,
    item_model: Uuid,
    item_parent: Option<Uuid>,
    root_actor: Uuid,
    item_actor: Uuid,
    item_actor_member: Option<Uuid>,
    item_execution_model: Uuid,
    source_is_root: bool,
    root_outputs: i64,
    conversation_memberships: i64,
}

#[tokio::test]
async fn accepted_response_output_survives_post_begin_conversation_tombstone() {
    let fixture = Fixture::new().await;
    let accepted = fixture.accepted_response().await;
    let removed = fixture.dp.send("POST", "/dp/v1/native-objects/tombstone", Some(json!({
        "api_key_id":fixture.keys[0],"record_id":accepted.conversation,"version":accepted.version
    }))).await;
    assert_eq!(
        removed.status, 200,
        "A legitimately tombstones C after accepting R"
    );
    let before: bool = sqlx::query_scalar(
        "SELECT deleted_at IS NOT NULL FROM native_objects WHERE environment_id=$1 AND id=$2",
    )
    .bind(fixture.cp.env().env_id)
    .bind(accepted.conversation)
    .fetch_one(fixture.cp.db().pool())
    .await
    .expect("explicit tombstone precondition");
    assert!(before);
    let bound = fixture.bind_response(&accepted).await;
    fixture.assert_durable_result(&accepted, &bound).await;
    let after: (bool,i64,i64) = sqlx::query_as("SELECT deleted_at IS NOT NULL,version,membership_epoch FROM native_objects WHERE environment_id=$1 AND id=$2")
        .bind(fixture.cp.env().env_id).bind(accepted.conversation).fetch_one(fixture.cp.db().pool()).await
        .expect("C tombstone remains unchanged after binding accepted outputs");
    assert_eq!(after, (true, accepted.version + 1, accepted.epoch));
}

#[tokio::test]
async fn accepted_response_output_survives_post_begin_temporary_conversation_fence() {
    let fixture = Fixture::new().await;
    let accepted = fixture.accepted_response().await;
    let reservation = Uuid::new_v4();
    let reserved = fixture.dp.send("POST", "/dp/v1/native-objects/relations/reserve", Some(json!({
        "api_key_id":fixture.keys[0],"operation_id":reservation,"parent_record_id":accepted.conversation,
        "expected_parent_version":accepted.version,"relation":"conversation_item","action":"attach",
        "child_record_ids":[]
    }))).await;
    assert_eq!(
        reserved.status, 200,
        "A legitimately reserves a C mutation after accepting R"
    );
    assert_eq!(reserved.body["state"], "reserved");
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM native_parent_fences WHERE environment_id=$1 AND parent_id=$2 AND operation_id=$3 AND state='held'")
        .bind(fixture.cp.env().env_id).bind(accepted.conversation).bind(reservation)
        .fetch_one(fixture.cp.db().pool()).await.expect("explicit live reservation precondition");
    assert_eq!(held, 1);
    let bound = fixture.bind_response(&accepted).await;
    fixture.assert_durable_result(&accepted, &bound).await;
    let unchanged: (bool,i64,i64,i64) = sqlx::query_as("SELECT deleted_at IS NULL,version,membership_epoch,(SELECT count(*) FROM native_parent_fences f WHERE f.environment_id=c.environment_id AND f.parent_id=c.id AND f.operation_id=$3 AND f.state='held') FROM native_objects c WHERE environment_id=$1 AND id=$2")
        .bind(fixture.cp.env().env_id).bind(accepted.conversation).bind(reservation)
        .fetch_one(fixture.cp.db().pool()).await.expect("C and its legitimate fence remain unchanged");
    assert_eq!(unchanged, (true, accepted.version, accepted.epoch, 1));
    // 通过正式拒绝结果释放本次预留，不以测试 SQL 清除或完成关系。
    let resolved = fixture
        .dp
        .send(
            "POST",
            "/dp/v1/native-objects/relations/resolve",
            Some(json!({
                "api_key_id":fixture.keys[0],"operation_id":reservation,"upstream_rejected":true
            })),
        )
        .await;
    assert_eq!(
        resolved.status, 200,
        "release the legitimate temporary fence"
    );
    assert_eq!(resolved.body["state"], "rejected");
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM native_parent_fences WHERE environment_id=$1 AND parent_id=$2 AND operation_id=$3 AND state IN ('held','uncertain')")
        .bind(fixture.cp.env().env_id).bind(accepted.conversation).bind(reservation)
        .fetch_one(fixture.cp.db().pool()).await.expect("temporary fence is no longer active");
    assert_eq!(active, 0);
    fixture.assert_durable_result(&accepted, &bound).await;
}
