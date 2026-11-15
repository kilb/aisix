//! 源码感知的真实 CP HTTP/Postgres 并发组件回归，不称为 source-blind 或 E2E。
//! 合法命令创建 C/R/I；控制事务仅锁 I，不改 owner、UUID、版本、关系或操作状态。
//! 两条产品路径共同以 C 优先后，合法 detach 预留与已接受 R 的绑定都应完成。

mod support;

use std::sync::Arc;
use std::time::Duration;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_core::native_objects::{
    configured_native_origin, native_origin_surface, platform_id_key_from_secret, NativeObjectKind,
    NativeProtocol,
};
use aisix_core::ProviderKey;
use serde_json::{json, Value};
use support::world::{seed_key, seed_tenant};
use support::{DpClient, OperatorSession, TestCp};
use tokio::time::Instant;
use uuid::Uuid;

const BEGIN: &str = "/dp/v1/native-objects/begin";
const BIND: &str = "/dp/v1/native-objects/bind";
const RESERVE: &str = "/dp/v1/native-objects/relations/reserve";
const COMMIT: &str = "/dp/v1/native-objects/relations/commit";

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
    dp: Arc<DpClient>,
    member: Uuid,
    key: Uuid,
    provider: Uuid,
    provider_doc: ProviderKey,
    models: [Uuid; 2],
}

struct Pair {
    conversation: Uuid,
    conversation_raw: String,
    root: Uuid,
    item: Uuid,
    operation: Uuid,
    root_raw: String,
    item_raw: String,
}

impl Fixture {
    async fn new() -> Self {
        assert!(std::env::var("CP_TEST_DATABASE_URL").is_ok());
        let cp = TestCp::start().await.expect("real isolated CP/Postgres");
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let doc = json!({"display_name":"conversation-lock-account","provider":"openai","adapter":"openai",
            "api_key":"sk-conversation-lock-component-fixture","api_base":"https://conversation-lock.example/v1",
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
                json!({"doc":{"display_name":format!("conversation-lock-model-{suffix}"),"provider":"openai",
                    "model_name":format!("conversation-lock-upstream-{suffix}"),"provider_key_id":provider}}),
            )
            .await;
            models.push(uuid_at(&model, "/id"));
        }
        let models = [models[0], models[1]];
        let tenant = seed_tenant(&cp, &admin).await;
        let path = format!("/admin/tenants/{tenant}");
        let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
        assert_eq!(current.status, 200, "component tenant lookup");
        let granted = cp
            .send_conditional(
                "PATCH",
                &path,
                admin.cookie(),
                current.body["version"].as_i64().expect("tenant version"),
                json!({"allowed_model_ids":models}),
            )
            .await;
        assert_eq!(granted.status, 200, "component model grant");
        let member = cp.seed_member(tenant, MemberRole::Member).await.id;
        let key = Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(member)).await)
            .expect("ordinary member key");
        let dp = Arc::new(cp.issue_dp_client("conversation-item-lock-order").await);
        Self {
            cp,
            dp,
            member,
            key,
            provider,
            provider_doc,
            models,
        }
    }

    async fn bind_new(
        &self,
        kind: NativeObjectKind,
        model: usize,
        parent: Option<Uuid>,
        raw: &str,
        item_raw: Option<&str>,
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
            "operation_id":operation,"api_key_id":self.key,"protocol":"openai","kind":kind,
            "origin":{"surface":native_origin_surface(NativeProtocol::Openai,kind),
                "model_id":self.models[model],"provider_key_id":self.provider,"boundary":boundary},
            "parent_id":parent,"requested_model_id":self.models[model],
            "requested_model":format!("conversation-lock-model-{suffix}"),
            "upstream_model":format!("conversation-lock-upstream-{suffix}"),"upstream_provider":"openai",
            "operation":if kind==NativeObjectKind::Conversation {"conversations"} else {"responses"},
            "request_id":format!("conversation-lock-{operation}"),"asynchronous":false
        }))).await;
        assert_eq!(begun.status, 200, "legal begin before lock race");
        let objects: Vec<Value> = item_raw
            .into_iter()
            .map(|raw| json!({"kind":"response_item","raw_id":raw}))
            .collect();
        let bound = self
            .dp
            .send(
                "POST",
                BIND,
                Some(json!({"operation_id":operation,"api_key_id":self.key,"raw_id":raw,"objects":objects})),
            )
            .await;
        assert_eq!(bound.status, 200, "legal bind before lock race");
        bound.body
    }

    async fn ordered_pair(&self) -> Pair {
        // 每次重建独立的合法 C/R/I；固定极小 C 反复择 I 会提高前置失败概率。
        for index in 0..64 {
            let conversation_raw = format!("conv_lock_{index}");
            let conversation = self
                .bind_new(
                    NativeObjectKind::Conversation,
                    0,
                    None,
                    &conversation_raw,
                    None,
                )
                .await;
            let conversation = uuid_at(&conversation, "/record/record_id");
            let root_raw = format!("resp_lock_{index}");
            let item_raw = format!("msg_lock_{index}");
            let root = self
                .bind_new(
                    NativeObjectKind::Response,
                    1,
                    Some(conversation),
                    &root_raw,
                    Some(&item_raw),
                )
                .await;
            let item = uuid_at(&root, "/records/0/record_id");
            let item_precedes_c: bool = sqlx::query_scalar("SELECT $1::uuid < $2::uuid")
                .bind(item)
                .bind(conversation)
                .fetch_one(self.cp.db().pool())
                .await
                .expect("read actual PostgreSQL UUID order");
            if item_precedes_c {
                return Pair {
                    conversation,
                    conversation_raw,
                    root: uuid_at(&root, "/record/record_id"),
                    item,
                    operation: uuid_at(&root, "/record/operation_id"),
                    root_raw,
                    item_raw,
                };
            }
        }
        panic!("no independent legal C/R/I pair satisfied the lock-order precondition");
    }
}

// 只识别 native_objects 的锁等待及阻塞 PID，不读取或输出 SQL、参数、正文与凭据。
async fn object_waiter(cp: &TestCp, blockers: &[i32], excluded: &[i32]) -> Option<i32> {
    tokio::time::timeout(Duration::from_millis(350), async {
        loop {
            let pids = sqlx::query_scalar::<_, i32>(
                "SELECT a.pid FROM pg_stat_activity a WHERE a.datname=current_database() AND a.wait_event_type='Lock' AND a.pid<>ALL($1::int4[]) AND pg_blocking_pids(a.pid) && $2::int4[] AND EXISTS(SELECT 1 FROM pg_locks l WHERE l.pid=a.pid AND l.relation='native_objects'::regclass)",
            )
            .bind(excluded)
            .bind(blockers)
            .fetch_all(cp.db().pool())
            .await
            .ok()?;
            if pids.len() == 1 {
                return Some(pids[0]);
            }
            if pids.len() > 1 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .ok()
    .flatten()
}

// UUID 排序若先锁 I，释放控制锁后便会与已经持 C 的 bind 形成环；两边都必须先锁 C。
#[tokio::test]
async fn conversation_item_reservation_and_accepted_bind_share_parent_first_lock_order() {
    let fixture = Fixture::new().await;
    let pair = fixture.ordered_pair().await;
    let conversation = fixture.dp.send("POST","/dp/v1/native-objects/lookup",Some(json!({
        "api_key_id":fixture.key,"locator":{"protocol":"openai","kind":"conversation","raw_id":pair.conversation_raw}
    }))).await;
    assert_eq!(conversation.status, 200, "current conversation version");
    assert_eq!(
        conversation.body["record"]["record_id"],
        json!(pair.conversation)
    );
    let version = conversation.body["record"]["version"]
        .as_i64()
        .expect("explicit parent version");
    assert!(version > 0);
    let before: (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE parent_id=$2 AND relation='response_output'),count(*) FILTER (WHERE parent_id=$3 AND relation='conversation_item') FROM native_object_memberships WHERE environment_id=$1 AND child_id=$4 AND deleted_at IS NULL",
    ).bind(fixture.cp.env().env_id).bind(pair.root).bind(pair.conversation).bind(pair.item)
        .fetch_one(fixture.cp.db().pool()).await.expect("legal existing memberships before detach");
    assert_eq!(
        before,
        (1, 1),
        "both live memberships must exist before the legal detach request"
    );

    let mut barrier = fixture
        .cp
        .db()
        .pool()
        .begin()
        .await
        .expect("read-only control transaction");
    let control_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *barrier)
        .await
        .expect("control backend identity");
    sqlx::query("SELECT id FROM native_objects WHERE environment_id=$1 AND id=$2 FOR UPDATE")
        .bind(fixture.cp.env().env_id)
        .bind(pair.item)
        .fetch_one(&mut *barrier)
        .await
        .expect("removable I-only control lock");
    let held_since = Instant::now();

    let detach_operation = Uuid::new_v4();
    let reserve_dp = Arc::clone(&fixture.dp);
    let reserve_body = json!({"api_key_id":fixture.key,"operation_id":detach_operation,
        "parent_record_id":pair.conversation,"expected_parent_version":version,
        "relation":"conversation_item","action":"detach","child_record_ids":[pair.item]});
    let mut reserve =
        tokio::spawn(async move { reserve_dp.send("POST", RESERVE, Some(reserve_body)).await });
    let reserve_pid = object_waiter(&fixture.cp, &[control_pid], &[control_pid]).await;

    // 不要求 bind 已持 C；新旧产品都只需真实请求进入同一已证实的等待链。
    let (mut bind, bind_pid) = if let Some(reserve_pid) = reserve_pid {
        let bind_dp = Arc::clone(&fixture.dp);
        let bind_body = json!({"operation_id":pair.operation,"api_key_id":fixture.key,
            "existing_record_id":pair.root,"raw_id":pair.root_raw,
            "objects":[{"kind":"response_item","raw_id":pair.item_raw}]});
        let bind = tokio::spawn(async move { bind_dp.send("POST", BIND, Some(bind_body)).await });
        let pid = object_waiter(
            &fixture.cp,
            &[control_pid, reserve_pid],
            &[control_pid, reserve_pid],
        )
        .await;
        (Some(bind), pid)
    } else {
        (None, None)
    };
    // 断言前总是释放 barrier 并收齐请求，前置失败也不遗留 I 控制锁。
    barrier
        .rollback()
        .await
        .expect("release I-only control lock");
    let held_for = held_since.elapsed();
    let mut reserve_reply = None;
    let mut bind_reply = None;
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        reserve_reply = Some((&mut reserve).await);
        if let Some(bind) = bind.as_mut() {
            bind_reply = Some(bind.await);
        }
    })
    .await;
    if completed.is_err() {
        // 只取消并收齐尚未 await 完成的任务，不丢弃仍在运行的 HTTP JoinHandle。
        if reserve_reply.is_none() {
            reserve.abort();
            let _ = (&mut reserve).await;
        }
        if bind_reply.is_none() {
            if let Some(bind) = bind.as_mut() {
                bind.abort();
                let _ = bind.await;
            }
        }
        panic!("HTTP tasks aborted and joined after the post-barrier deadline");
    }
    let reserved = reserve_reply
        .expect("reserve was awaited")
        .expect("reserve HTTP task completed");
    let bound = bind_reply.map(|reply| reply.expect("bind HTTP task completed"));
    eprintln!("[conversation-lock-order] control_identified={} observed_waiters={} reserve_waited={} bind_waited={} control_hold_ms={} reserve_status={} bind_status={}",
        control_pid>0,usize::from(reserve_pid.is_some())+usize::from(bind_pid.is_some()),
        reserve_pid.is_some(),bind_pid.is_some(),held_for.as_millis(),reserved.status,bound.as_ref().map_or(0,|response|response.status));
    assert!(
        held_for < Duration::from_secs(1),
        "I barrier must be released before the production statement budget"
    );
    assert!(
        reserve_pid.is_some(),
        "reserve must wait on the removable I lock on either product"
    );
    assert!(
        bind_pid.is_some(),
        "bind must reach the established lock chain on either product"
    );
    assert_eq!(
        reserved.status, 200,
        "legal detach reservation must complete without deadlock"
    );
    assert_eq!(reserved.body["state"], "reserved");
    let bound = bound.expect("bind reached the measured lock chain");
    assert_eq!(
        bound.status, 200,
        "temporary C fence must not erase accepted R/I facts"
    );
    assert_eq!(bound.body["record"]["record_id"], json!(pair.root));
    assert_eq!(bound.body["records"][0]["record_id"], json!(pair.item));
    assert_eq!(
        bound.body["record"]["origin"]["model_id"],
        json!(fixture.models[1])
    );
    assert_eq!(bound.body["records"][0]["api_key_id"], json!(fixture.key));
    assert_eq!(
        bound.body["records"][0]["origin"]["model_id"],
        json!(fixture.models[1])
    );

    let detached = fixture
        .dp
        .send(
            "POST",
            COMMIT,
            Some(json!({
                "api_key_id":fixture.key,"operation_id":detach_operation,"objects":[]
            })),
        )
        .await;
    assert_eq!(
        detached.status, 200,
        "commit the legal detach after both concurrent commands complete"
    );
    let facts: (Uuid,Uuid,Uuid,Option<Uuid>,bool,i64,i64) = sqlx::query_as(
        "SELECT item.api_key_id,item.member_id,item.model_id,item.parent_id,EXISTS(SELECT 1 FROM native_operation_origin_sources s WHERE s.environment_id=item.environment_id AND s.operation_id=item.operation_id AND s.source_record_id=$3),(SELECT count(*) FROM native_object_memberships m WHERE m.environment_id=item.environment_id AND m.parent_id=$3 AND m.child_id=item.id AND m.relation='response_output' AND m.deleted_at IS NULL),(SELECT count(*) FROM native_object_memberships m WHERE m.environment_id=item.environment_id AND m.parent_id=$4 AND m.child_id=item.id AND m.relation='conversation_item' AND m.deleted_at IS NULL) FROM native_objects item WHERE item.environment_id=$1 AND item.id=$2",
    ).bind(fixture.cp.env().env_id).bind(pair.item).bind(pair.root).bind(pair.conversation)
        .fetch_one(fixture.cp.db().pool()).await.expect("scoped read-only paid-fact and relationship counts");
    assert_eq!(
        facts,
        (
            fixture.key,
            fixture.member,
            fixture.models[1],
            Some(pair.root),
            true,
            1,
            0
        ),
        "I keeps its accepted owner, B execution and R source; detach affects only C membership"
    );
}
