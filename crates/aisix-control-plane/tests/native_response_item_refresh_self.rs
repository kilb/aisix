//! 源码感知的 CP HTTP/Postgres 组件回归；不称为 source-blind、供应商或网关 E2E。
//! 真实 begin/bind 建立 R/B 与 C/A 的产物关系，真实成员 API 将 B 从 Full 降为 Self。
//! 不模拟 registry、不直接修改角色数据库，也不改既有映射测试及诊断副本。

mod support;

use aisix_control_plane::auth::OperatorRole;
use aisix_core::native_objects::{
    configured_native_origin, native_origin_surface, platform_id_key_from_secret, NativeObjectKind,
    NativeProtocol,
};
use aisix_core::ProviderKey;
use serde_json::{json, Value};
use support::world::{group_id, seed_tenant};
use support::{DpClient, OperatorSession, Resp, TestCp, TEST_PASSWORD};
use uuid::Uuid;

const BEGIN: &str = "/dp/v1/native-objects/begin";
const BIND: &str = "/dp/v1/native-objects/bind";
const LOOKUP: &str = "/dp/v1/native-objects/lookup";
const CONVERSATION: &str = "conv_refresh_owner_a";
const RESPONSE: &str = "resp_refresh_actor_b";
const ITEM: &str = "msg_refresh_accepted_item_a";
const FOREIGN_RESPONSE: &str = "resp_refresh_unrelated_a";
const FOREIGN_ITEM: &str = "msg_refresh_unrelated_item_a";

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
    assert_eq!(response.status, 201, "POST {path}: {response:?}");
    response.body
}

struct Caller {
    member: Uuid,
    key: Uuid,
    cookie: String,
}

async fn caller(
    cp: &TestCp,
    admin: &OperatorSession,
    tenant: Uuid,
    role: &str,
    models: &[Uuid],
) -> Caller {
    let invited = create(
        cp,
        admin,
        &format!("/admin/tenants/{tenant}/invites"),
        json!({"email":format!("refresh-{role}-{}@tenant.test",Uuid::new_v4().simple()),"role":role}),
    )
    .await;
    let accepted = cp
        .send(
            "POST",
            "/api/invites/accept",
            None,
            Some(json!({"token":invited["invite_token"],"password":TEST_PASSWORD})),
        )
        .await;
    assert_eq!(accepted.status, 201, "accept member: {accepted:?}");
    assert_eq!(accepted.body["member"]["role"], role);
    assert_eq!(accepted.body["tenant"]["id"], json!(tenant));
    let member = uuid_at(&accepted.body, "/member/id");
    let cookie = accepted
        .set_cookie
        .as_deref()
        .and_then(|cookie| cookie.split(';').next())
        .expect("accepted member session")
        .to_owned();
    let key = create(
        cp,
        admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label":format!("refresh-{role}"),"member_id":member,
            "group_id":group_id(cp,"shared").await,"model_ids":models}),
    )
    .await;
    Caller {
        member,
        key: uuid_at(&key, "/key/id"),
        cookie,
    }
}

struct Fixture {
    cp: TestCp,
    dp: DpClient,
    tenant: Uuid,
    provider: Uuid,
    provider_doc: ProviderKey,
    models: [Uuid; 2],
    a: Caller,
    b: Caller,
    operation: Uuid,
    conversation: Uuid,
    root: Uuid,
    item: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        // 本目标不能在服务未配置时提前返回而误报通过。
        assert!(std::env::var("CP_TEST_DATABASE_URL").is_ok());
        let cp = TestCp::start().await.expect("real isolated CP/Postgres");
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let doc = json!({"display_name":"refresh-account","provider":"openai","adapter":"openai",
            "api_key":"sk-refresh-component-fixture","api_base":"https://refresh-component.example/v1",
            "groups":["shared"]});
        let provider_doc: ProviderKey =
            serde_json::from_value(doc.clone()).expect("component provider document");
        let provider = create(
            &cp,
            &admin,
            "/admin/resources/provider_keys",
            json!({"doc":doc}),
        )
        .await;
        let provider = uuid_at(&provider, "/id");
        let mut models = Vec::new();
        for suffix in ["a", "b"] {
            let model = create(
                &cp,
                &admin,
                "/admin/resources/models",
                json!({"doc":{"display_name":format!("refresh-model-{suffix}"),"provider":"openai",
                    "model_name":format!("refresh-upstream-{suffix}"),"provider_key_id":provider}}),
            )
            .await;
            models.push(uuid_at(&model, "/id"));
        }
        let models = [models[0], models[1]];
        let tenant = seed_tenant(&cp, &admin).await;
        let path = format!("/admin/tenants/{tenant}");
        let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
        assert_eq!(current.status, 200, "tenant: {current:?}");
        let granted = cp
            .send_conditional(
                "PATCH",
                &path,
                admin.cookie(),
                current.body["version"].as_i64().expect("tenant version"),
                json!({"allowed_model_ids":models}),
            )
            .await;
        assert_eq!(granted.status, 200, "model grant: {granted:?}");
        let a = caller(&cp, &admin, tenant, "member", &models).await;
        let b = caller(&cp, &admin, tenant, "admin", &models).await;
        let dp = cp.issue_dp_client("response-item-refresh-self").await;
        let mut fixture = Self {
            cp,
            dp,
            tenant,
            provider,
            provider_doc,
            models,
            a,
            b,
            operation: Uuid::nil(),
            conversation: Uuid::nil(),
            root: Uuid::nil(),
            item: Uuid::nil(),
        };
        let conversation = fixture
            .bind_new(
                fixture.a.key,
                NativeObjectKind::Conversation,
                0,
                None,
                CONVERSATION,
                &[],
            )
            .await;
        let conversation_id = uuid_at(&conversation, "/record/record_id");
        fixture.conversation = conversation_id;
        let mapped = fixture
            .bind_new(
                fixture.b.key,
                NativeObjectKind::Response,
                1,
                Some(conversation_id),
                RESPONSE,
                &[ITEM],
            )
            .await;
        fixture.operation = uuid_at(&mapped, "/record/operation_id");
        fixture.root = uuid_at(&mapped, "/record/record_id");
        let items = mapped["records"].as_array().expect("explicit item array");
        assert_eq!(items.len(), 1);
        fixture.item = uuid_at(&items[0], "/record_id");
        assert_eq!(mapped["record"]["api_key_id"], json!(fixture.b.key));
        assert_eq!(
            mapped["record"]["origin"]["model_id"],
            json!(fixture.models[1])
        );
        assert_eq!(mapped["record"]["parent_id"], json!(conversation_id));
        fixture.assert_item_owner_and_execution().await;
        let visible = fixture.lookup(fixture.b.key, "response_item", ITEM).await;
        assert_eq!(visible.status, 200, "Full sees accepted item: {visible:?}");
        fixture
    }

    async fn bind_new(
        &self,
        key: Uuid,
        kind: NativeObjectKind,
        model: usize,
        parent: Option<Uuid>,
        raw: &str,
        items: &[&str],
    ) -> Value {
        let operation = Uuid::new_v4();
        let boundary = configured_native_origin(
            &platform_id_key_from_secret(&self.cp.state().pki.platform_id_key_hex()),
            self.provider,
            &self.provider_doc,
            NativeProtocol::Openai,
            kind,
        )
        .expect("source-aware component origin");
        let suffix = ["a", "b"][model];
        let begun = self.dp.send("POST",BEGIN,Some(json!({
            "operation_id":operation,"api_key_id":key,"protocol":"openai","kind":kind,
            "origin":{"surface":native_origin_surface(NativeProtocol::Openai,kind),
                "model_id":self.models[model],"provider_key_id":self.provider,"boundary":boundary},
            "parent_id":parent,"requested_model_id":self.models[model],
            "requested_model":format!("refresh-model-{suffix}"),"upstream_model":format!("refresh-upstream-{suffix}"),
            "upstream_provider":"openai","operation":if kind==NativeObjectKind::Conversation {"conversations"} else {"responses"},
            "request_id":format!("refresh-{operation}"),"asynchronous":false
        }))).await;
        assert_eq!(begun.status, 200, "real begin: {begun:?}");
        assert_eq!(begun.body["state"], "pending_submission");
        let bound = self.dp.send("POST",BIND,Some(json!({
            "operation_id":operation,"api_key_id":key,"raw_id":raw,
            "objects":items.iter().map(|raw|json!({"kind":"response_item","raw_id":raw})).collect::<Vec<_>>()
        }))).await;
        assert_eq!(bound.status, 200, "real bind: {bound:?}");
        bound.body
    }

    async fn lookup(&self, key: Uuid, kind: &str, raw: &str) -> Resp {
        self.dp
            .send(
                "POST",
                LOOKUP,
                Some(json!({"api_key_id":key,"locator":{"protocol":"openai","kind":kind,"raw_id":raw}})),
            )
            .await
    }

    async fn refresh(&self, items: &[&str]) -> Resp {
        self.dp.send("POST",BIND,Some(json!({
            "operation_id":self.operation,"api_key_id":self.b.key,
            "existing_record_id":self.root,"raw_id":RESPONSE,
            "objects":items.iter().map(|raw|json!({"kind":"response_item","raw_id":raw})).collect::<Vec<_>>()
        }))).await
    }

    async fn demote_b(&self) {
        // B 的真实管理员会话自行降为 member；不使用只支持 disabled 的 operator PATCH。
        let changed = self
            .cp
            .send(
                "PATCH",
                &format!("/api/members/{}", self.b.member),
                Some(&self.b.cookie),
                Some(json!({"role":"member"})),
            )
            .await;
        assert_eq!(changed.status, 200, "real admin role update: {changed:?}");
        assert_eq!(changed.body["role"], "member");
        let root = self.lookup(self.b.key, "response", RESPONSE).await;
        assert_eq!(root.status, 200, "Self keeps its own R: {root:?}");
        assert_eq!(root.body["record"]["record_id"], json!(self.root));
        let private = self.lookup(self.b.key, "response_item", ITEM).await;
        assert_eq!(
            private.status, 404,
            "Self cannot directly read A's I: {private:?}"
        );
        let empty = self.refresh(&[]).await;
        assert_eq!(empty.status, 200, "owned R refresh control: {empty:?}");
        assert_eq!(empty.body["record"]["record_id"], json!(self.root));
    }

    async fn assert_item_owner_and_execution(&self) {
        let row: (Uuid, Uuid, Uuid, Uuid, Option<Uuid>) = sqlx::query_as(
            "SELECT tenant_id,api_key_id,member_id,model_id,parent_id FROM native_objects WHERE environment_id=$1 AND id=$2",
        ).bind(self.cp.env().env_id).bind(self.item).fetch_one(self.cp.db().pool()).await.expect("durable accepted item");
        assert_eq!(
            row,
            (
                self.tenant,
                self.a.key,
                self.a.member,
                self.models[1],
                Some(self.root)
            ),
            "I retains A owner and actual B model with physical parent R"
        );
        let owned = self.lookup(self.a.key, "response_item", ITEM).await;
        assert_eq!(owned.status, 200, "A keeps reading its item: {owned:?}");
        assert_eq!(owned.body["record"]["record_id"], json!(self.item));
        assert_eq!(
            owned.body["record"]["requested_model_id"],
            json!(self.models[1])
        );
        let edges: (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE parent_id=$2 AND relation='response_output'),count(*) FILTER (WHERE parent_id=$3 AND relation='conversation_item') FROM native_object_memberships WHERE environment_id=$1 AND child_id=$4 AND deleted_at IS NULL",
        ).bind(self.cp.env().env_id).bind(self.root).bind(self.conversation).bind(self.item)
            .fetch_one(self.cp.db().pool()).await.expect("accepted output and conversation memberships");
        assert_eq!(
            edges,
            (1, 1),
            "accepted item keeps both durable memberships"
        );
    }
}

// 将 R 的已接受产物误作直接读取 A 的对象，会违反这里的 200 合同。
#[tokio::test]
async fn a_full_to_self_actor_can_refresh_its_accepted_response_item_without_taking_ownership() {
    let fixture = Fixture::new().await;
    fixture.demote_b().await;
    let refreshed = fixture.refresh(&[ITEM]).await;
    assert_eq!(
        refreshed.status, 200,
        "Self refreshes its accepted R output: {refreshed:?}"
    );
    assert_eq!(refreshed.body["record"]["record_id"], json!(fixture.root));
    let items = refreshed.body["records"]
        .as_array()
        .expect("returned accepted items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["record_id"], json!(fixture.item));
    assert_eq!(items[0]["api_key_id"], json!(fixture.a.key));
    fixture.assert_item_owner_and_execution().await;
}

// 放宽已接受产物的 refresh 不能让同一 op 携带另一 R 的私有 I，失败也不得留下新关系。
#[tokio::test]
async fn a_self_refresh_cannot_attach_an_unrelated_private_item_to_the_same_operation() {
    let fixture = Fixture::new().await;
    let foreign = fixture
        .bind_new(
            fixture.a.key,
            NativeObjectKind::Response,
            0,
            None,
            FOREIGN_RESPONSE,
            &[FOREIGN_ITEM],
        )
        .await;
    let foreign_id = uuid_at(&foreign, "/records/0/record_id");
    let before = fixture
        .lookup(fixture.b.key, "response_item", FOREIGN_ITEM)
        .await;
    assert_eq!(
        before.status, 200,
        "Full initially sees foreign item: {before:?}"
    );
    fixture.demote_b().await;
    let private = fixture
        .lookup(fixture.b.key, "response_item", FOREIGN_ITEM)
        .await;
    assert_eq!(
        private.status, 404,
        "foreign I remains private: {private:?}"
    );
    let denied = fixture.refresh(&[FOREIGN_ITEM]).await;
    assert_eq!(
        denied.status, 404,
        "same op is not foreign-item authority: {denied:?}"
    );
    let attached: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM native_object_memberships WHERE environment_id=$1 AND child_id=$3 AND deleted_at IS NULL AND ((parent_id=$2 AND relation='response_output') OR (parent_id=$4 AND relation='conversation_item'))",
    ).bind(fixture.cp.env().env_id).bind(fixture.root).bind(foreign_id).bind(fixture.conversation)
        .fetch_one(fixture.cp.db().pool()).await.expect("no foreign output membership");
    assert_eq!(
        attached, 0,
        "a denied refresh cannot attach a foreign output or conversation item"
    );
    let root = fixture.lookup(fixture.b.key, "response", RESPONSE).await;
    assert_eq!(root.status, 200, "denial preserves B's R: {root:?}");
    fixture.assert_item_owner_and_execution().await;
}
