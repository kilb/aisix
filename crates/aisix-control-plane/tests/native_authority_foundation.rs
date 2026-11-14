//! 原生对象归属的真实 CP/Postgres 合同测试；不访问或模拟供应商。

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use ring::hmac;
use serde_json::{json, Value};
use support::world::{seed_key, seed_tenant};
use support::{DpClient, OperatorSession, Resp, TestCp};
use uuid::Uuid;

const BASE: &str = "https://native-foundation.example/v1";
const CREDENTIAL: &str = "sk-foundation-fixture-v12";
const RESPONSE_ID: &str = "resp_raw-00.V12";
const ITEM_ID: &str = "msg_raw-00.V12";
const CONTAINER_ID: &str = "cntr_raw-00.V12";
const CHILDREN: [(&str, &str); 2] = [
    ("response_item", ITEM_ID),
    ("response_container", CONTAINER_ID),
];

fn uuid_at(value: &Value, pointer: &str) -> Uuid {
    Uuid::parse_str(
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("missing UUID at {pointer}: {value}")),
    )
    .expect("an explicit UUID")
}

// 直接编码既有指纹线协议，不调用产品的 origin helper 来制造自证结果。
fn boundary(cp: &TestCp, provider: Uuid) -> String {
    let secret = ring::digest::digest(
        &ring::digest::SHA256,
        cp.state().pki.platform_id_key_hex().as_bytes(),
    );
    let mut input = b"aisix-native-origin-v1\0".to_vec();
    let provider = provider.to_string();
    for part in [provider.as_str(), "openai", "openai", BASE, CREDENTIAL] {
        input.extend_from_slice(&(part.len() as u64).to_be_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    input.extend_from_slice(&0_u64.to_be_bytes());
    format!(
        "v1:{}",
        hex::encode(
            hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret.as_ref()), &input).as_ref()
        )
    )
}

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    dp: DpClient,
    tenant: Uuid,
    member: Uuid,
    key: Uuid,
    submission: Value,
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let provider = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name":"foundation-account-v12", "provider":"openai",
                "api_key":CREDENTIAL, "api_base":BASE, "groups":["shared"]
            }})),
        )
        .await;
    assert_eq!(provider.status, 201, "{provider:?}");
    let provider = uuid_at(&provider.body, "/id");
    let model = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name":"foundation-model-v12", "provider":"openai",
                "model_name":"gpt-4o-mini", "provider_key_id":provider
            }})),
        )
        .await;
    assert_eq!(model.status, 201, "{model:?}");
    let model = uuid_at(&model.body, "/id");
    let tenant = seed_tenant(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[model]).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let key = Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(member)).await)
        .expect("the original member's key");
    let dp = cp.issue_dp_client("foundation-creator-v12").await;
    let submission = json!({
        "operation_id":Uuid::new_v4(), "api_key_id":key,
        "protocol":"openai", "kind":"response",
        "origin":{"model_id":model,"provider_key_id":provider,"boundary":boundary(&cp, provider)},
        "requested_model_id":model, "requested_model":"foundation-model-v12",
        "upstream_model":"gpt-4o-mini", "operation":"responses",
        "request_id":"foundation-request-v12", "asynchronous":false
    });
    Some(Fixture {
        cp,
        admin,
        dp,
        tenant,
        member,
        key,
        submission,
    })
}

async fn grant(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, models: &[Uuid]) {
    let path = format!("/admin/tenants/{tenant}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let changed = cp
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            current.body["version"]
                .as_i64()
                .expect("the tenant version"),
            json!({"allowed_model_ids":models}),
        )
        .await;
    assert_eq!(changed.status, 200, "{changed:?}");
}

async fn bind_legacy_response(dp: &DpClient, submission: &Value, raw: &str) -> Value {
    let begun = dp
        .send(
            "POST",
            "/dp/v1/native-objects/begin",
            Some(submission.clone()),
        )
        .await;
    assert_eq!(begun.status, 200, "{begun:?}");
    // 故意只发送升级前 Bind DTO 已有字段，覆盖滚动期间旧 DP 的新写入。
    let bound = dp
        .send(
            "POST",
            "/dp/v1/native-objects/bind",
            Some(json!({
                "operation_id":submission["operation_id"], "api_key_id":submission["api_key_id"],
                "raw_id":raw, "objects":[
                    {"kind":"response_item","raw_id":ITEM_ID},
                    {"kind":"response_container","raw_id":CONTAINER_ID}
                ]
            })),
        )
        .await;
    assert_eq!(bound.status, 200, "{bound:?}");
    assert_eq!(bound.body["record"]["raw_id"].as_str(), Some(raw));
    let children = bound.body["records"].as_array().expect("explicit children");
    assert_eq!(children.len(), CHILDREN.len());
    for (kind, id) in CHILDREN {
        let child = children
            .iter()
            .find(|child| child["kind"].as_str() == Some(kind))
            .expect("each requested child kind");
        assert_eq!(child["raw_id"].as_str(), Some(id));
        assert_eq!(child["parent_id"], bound.body["record"]["record_id"]);
    }
    bound.body
}

async fn lookup(dp: &DpClient, key: Uuid, kind: &str, raw: &str) -> Resp {
    dp.send(
        "POST",
        "/dp/v1/native-objects/lookup",
        Some(json!({"api_key_id":key,"locator":{
            "protocol":"openai","kind":kind,"raw_id":raw
        }})),
    )
    .await
}

async fn bind_root(dp: &DpClient, submission: &Value, raw: &str) -> Value {
    let begun = dp
        .send(
            "POST",
            "/dp/v1/native-objects/begin",
            Some(submission.clone()),
        )
        .await;
    assert_eq!(begun.status, 200, "{begun:?}");
    let bound = dp.send("POST", "/dp/v1/native-objects/bind", Some(json!({
        "operation_id":submission["operation_id"], "api_key_id":submission["api_key_id"], "raw_id":raw
    }))).await;
    assert_eq!(bound.status, 200, "{bound:?}");
    assert_eq!(bound.body["record"]["raw_id"].as_str(), Some(raw));
    bound.body["record"].clone()
}

fn root_submission(f: &Fixture, kind: &str) -> Value {
    let mut submission = f.submission.clone();
    submission["operation_id"] = json!(Uuid::new_v4());
    submission["kind"] = json!(kind);
    submission["operation"] = json!(kind);
    submission
}

async fn delete_parent(dp: &DpClient, key: Uuid, record: &Value) {
    let deleted = dp
        .send(
            "POST",
            "/dp/v1/native-objects/tombstone",
            Some(json!({
                "api_key_id":key,"record_id":record["record_id"],"version":record["version"]
            })),
        )
        .await;
    assert_eq!(deleted.status, 200, "{deleted:?}");
    let parent = lookup(dp, key, "response", RESPONSE_ID).await;
    assert_eq!(
        parent.status, 404,
        "the Response itself was removed: {parent:?}"
    );
}

async fn assert_children_visible(dp: &DpClient, key: Uuid, parent: &Value) {
    for (kind, raw) in CHILDREN {
        let found = lookup(dp, key, kind, raw).await;
        assert_eq!(
            found.status, 200,
            "{kind} survives its provenance Response: {found:?}"
        );
        assert_eq!(found.body["record"]["raw_id"].as_str(), Some(raw));
        assert_eq!(found.body["record"]["parent_id"], parent["record_id"]);
        assert_eq!(found.body["record"]["api_key_id"], json!(key));
    }
}

// 删除后仍读取成功才证明 parent 不是授权依赖；只检查 parent 字段存在不能证明此事。
#[tokio::test]
async fn legacy_response_children_keep_provenance_without_inheriting_parent_deletion() {
    let Some(f) = fixture().await else { return };
    let bound = bind_legacy_response(&f.dp, &f.submission, RESPONSE_ID).await;
    assert_children_visible(&f.dp, f.key, &bound["record"]).await;
    delete_parent(&f.dp, f.key, &bound["record"]).await;
    let restarted = f.cp.restart().await;
    let reader = restarted
        .issue_dp_client("foundation-reader-after-restart-v12")
        .await;
    assert_children_visible(&reader, f.key, &bound["record"]).await;

    let foreign_member = restarted.seed_member(f.tenant, MemberRole::Member).await.id;
    let foreign_key =
        Uuid::parse_str(&seed_key(&restarted, &f.admin, f.tenant, Some(foreign_member)).await)
            .expect("another ordinary member's key");
    for (kind, raw) in CHILDREN {
        let foreign = lookup(&reader, foreign_key, kind, raw).await;
        let missing = lookup(&reader, foreign_key, kind, "unknown_raw_v12").await;
        assert_eq!(foreign.status, 404, "{foreign:?}");
        assert_eq!(foreign.body, missing.body);
        let owned = reader
            .send(
                "POST",
                "/dp/v1/native-objects/owned",
                Some(json!({
                    "api_key_id":f.key,"protocol":"openai","kind":kind,
                    "origin":f.submission["origin"],"raw_ids":["unknown_raw_v12",raw]
                })),
            )
            .await;
        assert_eq!(owned.status, 200, "{owned:?}");
        let records = owned.body["records"]
            .as_array()
            .expect("explicit owned records");
        assert_eq!(records.len(), 1, "{owned:?}");
        assert_eq!(records[0]["raw_id"].as_str(), Some(raw));
    }
}

// provenance 分离不能放宽对象自己的当前 key、member 或原模型授权。
#[tokio::test]
async fn independent_response_children_still_require_current_key_member_and_model_grant() {
    let Some(f) = fixture().await else { return };
    let bound = bind_legacy_response(&f.dp, &f.submission, RESPONSE_ID).await;
    delete_parent(&f.dp, f.key, &bound["record"]).await;
    for (close, restore, id) in [
        (
            "UPDATE tenant_keys SET disabled=true WHERE id=$1",
            "UPDATE tenant_keys SET disabled=false WHERE id=$1",
            f.key,
        ),
        (
            "UPDATE members SET disabled=true WHERE id=$1",
            "UPDATE members SET disabled=false WHERE id=$1",
            f.member,
        ),
    ] {
        assert_children_visible(&f.dp, f.key, &bound["record"]).await;
        sqlx::query(close)
            .bind(id)
            .execute(f.cp.db().pool())
            .await
            .expect("withdraw current admission");
        for (kind, raw) in CHILDREN {
            let refused = lookup(&f.dp, f.key, kind, raw).await;
            assert_eq!(refused.status, 404, "{close}: {refused:?}");
        }
        sqlx::query(restore)
            .bind(id)
            .execute(f.cp.db().pool())
            .await
            .expect("restore current admission");
    }
    assert_children_visible(&f.dp, f.key, &bound["record"]).await;
    grant(&f.cp, &f.admin, f.tenant, &[]).await;
    for (kind, raw) in CHILDREN {
        let refused = lookup(&f.dp, f.key, kind, raw).await;
        assert_eq!(
            refused.status, 404,
            "removed original model grant: {refused:?}"
        );
    }
    grant(
        &f.cp,
        &f.admin,
        f.tenant,
        &[uuid_at(&f.submission, "/origin/model_id")],
    )
    .await;
    assert_children_visible(&f.dp, f.key, &bound["record"]).await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id=$1")
        .bind(f.key)
        .fetch_one(f.cp.db().pool())
        .await
        .expect("the live key version");
    let revoked =
        f.cp.send_conditional(
            "DELETE",
            &format!("/admin/keys/{}", f.key),
            f.admin.cookie(),
            version,
            Value::Null,
        )
        .await;
    assert_eq!(revoked.status, 204, "{revoked:?}");
    for (kind, raw) in CHILDREN {
        let refused = lookup(&f.dp, f.key, kind, raw).await;
        assert_eq!(refused.status, 404, "revoked original key: {refused:?}");
    }
}

// parent 已删除后，child 仍以自己的原模型重验，不能只验证一个当前可用账号。
#[tokio::test]
async fn independent_response_children_reject_a_deleted_original_model() {
    let Some(f) = fixture().await else { return };
    let bound = bind_legacy_response(&f.dp, &f.submission, RESPONSE_ID).await;
    delete_parent(&f.dp, f.key, &bound["record"]).await;
    assert_children_visible(&f.dp, f.key, &bound["record"]).await;
    let path = format!(
        "/admin/resources/models/{}",
        uuid_at(&f.submission, "/origin/model_id")
    );
    let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let deleted =
        f.cp.send_conditional(
            "DELETE",
            &path,
            f.admin.cookie(),
            current.body["version"].as_i64().expect("the model version"),
            Value::Null,
        )
        .await;
    assert_eq!(deleted.status, 200, "{deleted:?}");
    for (kind, raw) in CHILDREN {
        let refused = lookup(&f.dp, f.key, kind, raw).await;
        assert_eq!(refused.status, 404, "deleted original model: {refused:?}");
    }
}

// 只删除 parent 检查会变绿；漏掉 child 自身 origin 检查会让任一轮换仍可读。
#[tokio::test]
async fn independent_response_children_reject_changed_credentials_and_canonical_base() {
    let Some(f) = fixture().await else { return };
    let bound = bind_legacy_response(&f.dp, &f.submission, RESPONSE_ID).await;
    delete_parent(&f.dp, f.key, &bound["record"]).await;
    let path = format!(
        "/admin/resources/provider_keys/{}",
        uuid_at(&f.submission, "/origin/provider_key_id")
    );
    for (field, changed_value) in [
        ("api_key", "sk-foundation-rotated-v12"),
        ("api_base", "https://native-foundation-other.example/v1"),
    ] {
        assert_children_visible(&f.dp, f.key, &bound["record"]).await;
        for changed in [true, false] {
            let current = f.cp.send("GET", &path, Some(f.admin.cookie()), None).await;
            assert_eq!(current.status, 200, "{current:?}");
            let mut doc = current.body["doc"].clone();
            doc["api_key"] = json!(CREDENTIAL);
            doc["api_base"] = json!(BASE);
            if changed {
                doc[field] = json!(changed_value);
            }
            let saved =
                f.cp.send_conditional(
                    "PUT",
                    &path,
                    f.admin.cookie(),
                    current.body["version"].as_i64().expect("account version"),
                    json!({"doc":doc}),
                )
                .await;
            assert_eq!(saved.status, 200, "{saved:?}");
            if changed {
                for (kind, raw) in CHILDREN {
                    let refused = lookup(&f.dp, f.key, kind, raw).await;
                    assert_eq!(refused.status, 409, "{field} rotation: {refused:?}");
                    assert_eq!(refused.body["code"].as_str(), Some("native_origin_changed"));
                }
            }
        }
    }
    assert_children_visible(&f.dp, f.key, &bound["record"]).await;
}

// 允许独立产物不等于忽略所有 parent：原本依赖 parent 的 File 仍随父授权收紧。
#[tokio::test]
async fn provenance_exception_does_not_remove_a_files_authorization_dependency() {
    let Some(f) = fixture().await else { return };
    let mut submission = root_submission(&f, "message_batch");
    submission["asynchronous"] = json!(true);
    submission["entries"] = json!([{
        "entry_id":"batch-entry-v12", "endpoint":"/v1/responses",
        "requested_model_id":f.submission["requested_model_id"],
        "model_id":f.submission["origin"]["model_id"], "upstream_model":"gpt-4o-mini"
    }]);
    let batch = bind_root(&f.dp, &submission, "batch_dependency_v12").await;
    let claimed =
        f.dp.send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({
                "limit":1,"record_id":batch["record_id"]
            })),
        )
        .await;
    assert_eq!(claimed.status, 200, "{claimed:?}");
    let lease = claimed.body["records"]
        .as_array()
        .expect("explicit claimed records");
    assert_eq!(lease.len(), 1);
    let generation = lease[0]["lease_generation"]
        .as_i64()
        .expect("explicit lease generation");
    let completed = f.dp.send("POST", "/dp/v1/native-objects/complete", Some(json!({
        "lease":{"record_id":batch["record_id"],"generation":generation},
        "terminal_status":"completed","output_file_ids":["file_dependency_v12"],
        "usage":[{"entry_id":"batch-entry-v12","occurred_at":chrono::Utc::now().to_rfc3339(),
            "status_code":200,"prompt_tokens":9,"completion_tokens":2,"total_tokens":11}]
    }))).await;
    assert_eq!(completed.status, 200, "{completed:?}");
    let before = lookup(&f.dp, f.key, "file", "file_dependency_v12").await;
    assert_eq!(before.status, 200, "{before:?}");
    assert_eq!(before.body["record"]["parent_id"], batch["record_id"]);
    let current = lookup(&f.dp, f.key, "message_batch", "batch_dependency_v12").await;
    assert_eq!(current.status, 200, "{current:?}");
    let deleted = f.dp.send("POST", "/dp/v1/native-objects/tombstone", Some(json!({
        "api_key_id":f.key,"record_id":batch["record_id"],"version":current.body["record"]["version"]
    }))).await;
    assert_eq!(deleted.status, 200, "{deleted:?}");
    let after = lookup(&f.dp, f.key, "file", "file_dependency_v12").await;
    assert_eq!(
        after.status, 404,
        "a dependent File stays parent-authorized: {after:?}"
    );
}

async fn reserve_relation(
    dp: &DpClient,
    key: Uuid,
    operation: Uuid,
    parent: &Value,
    relation: &str,
    action: &str,
    children: &[Uuid],
) -> Resp {
    dp.send(
        "POST",
        "/dp/v1/native-objects/relations/reserve",
        Some(json!({
            "api_key_id":key,"operation_id":operation,"parent_record_id":parent["record_id"],
            "expected_parent_version":parent["version"],"relation":relation,
            "action":action,"child_record_ids":children
        })),
    )
    .await
}

async fn commit_relation(dp: &DpClient, key: Uuid, operation: Uuid, objects: Value) -> Resp {
    dp.send(
        "POST",
        "/dp/v1/native-objects/relations/commit",
        Some(json!({
            "api_key_id":key,"operation_id":operation,"objects":objects
        })),
    )
    .await
}

async fn related(dp: &DpClient, key: Uuid, parent: &Value, relation: &str, raw: &str) -> Resp {
    let response = dp.send("POST", "/dp/v1/native-objects/relations/lookup", Some(json!({
        "api_key_id":key,"parent_record_id":parent["record_id"],"relation":relation,"raw_ids":[raw]
    }))).await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.body["state"].as_str(), Some("related"));
    response
}

fn one_membership(response: &Resp, parent: &Value, child: &Value, relation: &str) -> Value {
    let records = response.body["records"]
        .as_array()
        .expect("explicit related objects");
    let memberships = response.body["memberships"]
        .as_array()
        .expect("explicit memberships");
    assert_eq!(records.len(), 1, "{response:?}");
    assert_eq!(memberships.len(), 1, "{response:?}");
    assert_eq!(records[0]["record_id"], child["record_id"]);
    assert_eq!(records[0]["raw_id"], child["raw_id"]);
    assert_eq!(memberships[0]["parent_record_id"], parent["record_id"]);
    assert_eq!(memberships[0]["child_record_id"], child["record_id"]);
    assert_eq!(memberships[0]["relation"].as_str(), Some(relation));
    assert!(
        memberships[0]["generation"]
            .as_i64()
            .expect("explicit generation")
            > 0
    );
    assert!(
        memberships[0]["version"]
            .as_i64()
            .expect("explicit version")
            > 0
    );
    uuid_at(&memberships[0], "/membership_id");
    memberships[0].clone()
}

fn no_membership(response: &Resp) {
    assert!(
        response.body["records"]
            .as_array()
            .expect("explicit empty related objects")
            .is_empty(),
        "{response:?}"
    );
    assert!(
        response.body["memberships"]
            .as_array()
            .expect("explicit empty memberships")
            .is_empty(),
        "{response:?}"
    );
}

// B 的 Full 可见性允许可信父读取，但新发现的产物必须归原始 Self 创建者 A。
#[tokio::test]
async fn anchored_discovery_inherits_parent_owner_instead_of_the_full_reader() {
    let Some(f) = fixture().await else { return };
    let parent = bind_root(
        &f.dp,
        &root_submission(&f, "conversation"),
        "conv_self_owner_v12",
    )
    .await;
    let reader_member = f.cp.seed_member(f.tenant, MemberRole::Admin).await.id;
    let reader_key =
        Uuid::parse_str(&seed_key(&f.cp, &f.admin, f.tenant, Some(reader_member)).await)
            .expect("the Full reader key");
    let raw = "msg_parent_discovered_exact_v12";
    let absent = lookup(&f.dp, f.key, "response_item", raw).await;
    assert_eq!(
        absent.status, 404,
        "the child was not previously registered: {absent:?}"
    );
    let parent_read = lookup(&f.dp, reader_key, "conversation", "conv_self_owner_v12").await;
    assert_eq!(
        parent_read.status, 200,
        "Full reader currently sees A's parent: {parent_read:?}"
    );
    let operation = Uuid::new_v4();
    let reserved = reserve_relation(
        &f.dp,
        reader_key,
        operation,
        &parent_read.body["record"],
        "conversation_item",
        "discover",
        &[],
    )
    .await;
    assert_eq!(reserved.status, 200, "{reserved:?}");
    assert_eq!(reserved.body["state"].as_str(), Some("reserved"));
    let objects = json!([{"kind":"response_item","raw_id":raw}]);
    for _ in 0..2 {
        let committed = commit_relation(&f.dp, reader_key, operation, objects.clone()).await;
        assert_eq!(committed.status, 200, "{committed:?}");
        assert_eq!(committed.body["state"].as_str(), Some("committed"));
    }
    let restarted = f.cp.restart().await;
    let second_dp = restarted
        .issue_dp_client("foundation-discovery-reader-v12")
        .await;
    let owner_read = lookup(&second_dp, f.key, "response_item", raw).await;
    assert_eq!(
        owner_read.status, 200,
        "Self creator retains its discovered child: {owner_read:?}"
    );
    assert_eq!(owner_read.body["record"]["raw_id"].as_str(), Some(raw));
    assert_eq!(owner_read.body["record"]["api_key_id"], json!(f.key));
    assert_eq!(owner_read.body["record"]["origin"], parent["origin"]);
    let membership = related(&second_dp, f.key, &parent, "conversation_item", raw).await;
    one_membership(
        &membership,
        &parent,
        &owner_read.body["record"],
        "conversation_item",
    );
    sqlx::query("UPDATE members SET role='member' WHERE id=$1")
        .bind(reader_member)
        .execute(restarted.db().pool())
        .await
        .expect("withdraw the reader's Full visibility");
    let former_full = lookup(&second_dp, reader_key, "response_item", raw).await;
    assert_eq!(
        former_full.status, 404,
        "discovery does not transfer ownership to B: {former_full:?}"
    );
    let owner_after = lookup(&second_dp, f.key, "response_item", raw).await;
    assert_eq!(owner_after.status, 200, "{owner_after:?}");
    assert_eq!(
        owner_after.body["record"]["record_id"],
        owner_read.body["record"]["record_id"]
    );
}

// 两个 store 共享同一 File；旧快照发现不能复活 detach，重试旧写也不能删除新代关系。
#[tokio::test]
async fn stale_discovery_cannot_revive_detached_membership_or_affect_another_store() {
    let Some(f) = fixture().await else { return };
    let raw = "file_many_to_many_exact_v12";
    let file = bind_root(&f.dp, &root_submission(&f, "file"), raw).await;
    let child_id = uuid_at(&file, "/record_id");
    let first = bind_root(&f.dp, &root_submission(&f, "vector_store"), "vs_first_v12").await;
    let second = bind_root(&f.dp, &root_submission(&f, "vector_store"), "vs_second_v12").await;
    let mut memberships = Vec::new();
    for parent in [&first, &second] {
        let operation = Uuid::new_v4();
        let reserved = reserve_relation(
            &f.dp,
            f.key,
            operation,
            parent,
            "vector_store_file",
            "attach",
            &[child_id],
        )
        .await;
        assert_eq!(reserved.status, 200, "{reserved:?}");
        let committed = commit_relation(&f.dp, f.key, operation, json!([])).await;
        assert_eq!(committed.status, 200, "{committed:?}");
        let result = related(&f.dp, f.key, parent, "vector_store_file", raw).await;
        memberships.push(one_membership(&result, parent, &file, "vector_store_file"));
    }
    let current = lookup(&f.dp, f.key, "vector_store", "vs_first_v12").await;
    assert_eq!(current.status, 200, "{current:?}");
    let discovery = Uuid::new_v4();
    let reserved = reserve_relation(
        &f.dp,
        f.key,
        discovery,
        &current.body["record"],
        "vector_store_file",
        "discover",
        &[],
    )
    .await;
    assert_eq!(reserved.status, 200, "{reserved:?}");
    let detach = Uuid::new_v4();
    let detaching = reserve_relation(
        &f.dp,
        f.key,
        detach,
        &current.body["record"],
        "vector_store_file",
        "detach",
        &[child_id],
    )
    .await;
    assert_eq!(
        detaching.status, 200,
        "the read snapshot must not block a write: {detaching:?}"
    );
    let detached = commit_relation(&f.dp, f.key, detach, json!([])).await;
    assert_eq!(detached.status, 200, "{detached:?}");
    let late = commit_relation(
        &f.dp,
        f.key,
        discovery,
        json!([{"kind":"file","raw_id":raw}]),
    )
    .await;
    assert_eq!(
        late.status, 409,
        "an old parent epoch cannot recreate the relation: {late:?}"
    );
    no_membership(&related(&f.dp, f.key, &first, "vector_store_file", raw).await);
    let unaffected = related(&f.dp, f.key, &second, "vector_store_file", raw).await;
    assert_eq!(
        one_membership(&unaffected, &second, &file, "vector_store_file"),
        memberships[1]
    );
    let file_read = lookup(&f.dp, f.key, "file", raw).await;
    assert_eq!(
        file_read.status, 200,
        "detach does not delete the source File: {file_read:?}"
    );
    assert_eq!(file_read.body["record"]["record_id"], file["record_id"]);

    let restarted = f.cp.restart().await;
    let second_dp = restarted
        .issue_dp_client("foundation-membership-recovery-v12")
        .await;
    no_membership(&related(&second_dp, f.key, &first, "vector_store_file", raw).await);
    let current = lookup(&second_dp, f.key, "vector_store", "vs_first_v12").await;
    assert_eq!(current.status, 200, "{current:?}");
    let attach = Uuid::new_v4();
    let reserved = reserve_relation(
        &second_dp,
        f.key,
        attach,
        &current.body["record"],
        "vector_store_file",
        "attach",
        &[child_id],
    )
    .await;
    assert_eq!(reserved.status, 200, "{reserved:?}");
    let committed = commit_relation(&second_dp, f.key, attach, json!([])).await;
    assert_eq!(committed.status, 200, "{committed:?}");
    let renewed = related(&second_dp, f.key, &first, "vector_store_file", raw).await;
    let renewed_membership = one_membership(&renewed, &first, &file, "vector_store_file");
    assert!(
        renewed_membership["generation"]
            .as_i64()
            .expect("new generation")
            > memberships[0]["generation"]
                .as_i64()
                .expect("old generation")
    );
    assert_ne!(
        renewed_membership["membership_id"],
        memberships[0]["membership_id"]
    );
    let repeated_detach = commit_relation(&f.dp, f.key, detach, json!([])).await;
    assert_eq!(
        repeated_detach.status, 200,
        "old commits replay their receipt: {repeated_detach:?}"
    );
    let after_replay = related(&second_dp, f.key, &first, "vector_store_file", raw).await;
    assert_eq!(
        one_membership(&after_replay, &first, &file, "vector_store_file"),
        renewed_membership
    );
    let other = related(&second_dp, f.key, &second, "vector_store_file", raw).await;
    assert_eq!(
        one_membership(&other, &second, &file, "vector_store_file"),
        memberships[1]
    );
}

// 同一对象换到可信观察入口，也不能绕过当前已经生效的读取拒绝。
fn discovery_does_not_release_unreadable_child(response: &Resp, raw: &str) {
    match response.status {
        200 => no_membership(response),
        404 => assert_eq!(
            response.body["code"].as_str(),
            Some("not_found"),
            "{response:?}"
        ),
        _ => {
            panic!("discovery must omit an unreadable child or refuse it as missing: {response:?}")
        }
    }
    assert!(
        !response.body.to_string().contains(raw),
        "an unreadable child ID was released: {response:?}"
    );
}

// 已有关联不替代当前模型权限：撤销 B 后，A 的集合发现也不能释放 B 的文件。
#[tokio::test]
async fn discovery_of_an_existing_membership_rechecks_the_childs_current_model_grant() {
    let Some(f) = fixture().await else { return };
    let model_a = uuid_at(&f.submission, "/origin/model_id");
    let model_b = f.cp.send("POST", "/admin/resources/models", Some(f.admin.cookie()), Some(json!({"doc":{
        "display_name":"discovery-child-model-b-v14", "provider":"openai",
        "model_name":"discovery-child-upstream-b-v14", "provider_key_id":f.submission["origin"]["provider_key_id"]
    }}))).await;
    assert_eq!(model_b.status, 201, "{model_b:?}");
    let model_b = uuid_at(&model_b.body, "/id");
    grant(&f.cp, &f.admin, f.tenant, &[model_a, model_b]).await;
    let raw_file = "file_discovery_revoked_model_b_v14";
    let mut file_submission = root_submission(&f, "file");
    file_submission["origin"]["model_id"] = json!(model_b);
    file_submission["requested_model_id"] = json!(model_b);
    file_submission["requested_model"] = json!("discovery-child-model-b-v14");
    file_submission["upstream_model"] = json!("discovery-child-upstream-b-v14");
    let file = bind_root(&f.dp, &file_submission, raw_file).await;
    let full_member = f.cp.seed_member(f.tenant, MemberRole::Admin).await.id;
    let full_key = Uuid::parse_str(&seed_key(&f.cp, &f.admin, f.tenant, Some(full_member)).await)
        .expect("the Full collection owner's key");
    let raw_store = "vs_discovery_owner_model_a_v14";
    let mut store_submission = root_submission(&f, "vector_store");
    store_submission["api_key_id"] = json!(full_key);
    let store = bind_root(&f.dp, &store_submission, raw_store).await;
    let visible = lookup(&f.dp, full_key, "file", raw_file).await;
    assert_eq!(
        visible.status, 200,
        "Full can read B before withdrawal: {visible:?}"
    );
    assert_eq!(visible.body["record"]["record_id"], file["record_id"]);
    let attach = Uuid::new_v4();
    let reserved = reserve_relation(
        &f.dp,
        full_key,
        attach,
        &store,
        "vector_store_file",
        "attach",
        &[uuid_at(&file, "/record_id")],
    )
    .await;
    assert_eq!(reserved.status, 200, "{reserved:?}");
    let attached = commit_relation(&f.dp, full_key, attach, json!([])).await;
    assert_eq!(attached.status, 200, "{attached:?}");
    one_membership(
        &related(&f.dp, full_key, &store, "vector_store_file", raw_file).await,
        &store,
        &file,
        "vector_store_file",
    );
    let parent = lookup(&f.dp, full_key, "vector_store", raw_store).await;
    assert_eq!(parent.status, 200, "{parent:?}");
    let parent = parent.body["record"].clone();
    parent["version"]
        .as_i64()
        .expect("the explicit current parent version after attachment");

    grant(&f.cp, &f.admin, f.tenant, &[model_a]).await;
    let denied = lookup(&f.dp, full_key, "file", raw_file).await;
    assert_eq!(
        denied.status, 404,
        "B's file is no longer readable: {denied:?}"
    );
    no_membership(&related(&f.dp, full_key, &parent, "vector_store_file", raw_file).await);
    let current_parent = lookup(&f.dp, full_key, "vector_store", raw_store).await;
    assert_eq!(
        current_parent.status, 200,
        "A's collection remains readable: {current_parent:?}"
    );
    assert_eq!(current_parent.body["record"]["version"], parent["version"]);
    let discover = Uuid::new_v4();
    let reserved = reserve_relation(
        &f.dp,
        full_key,
        discover,
        &parent,
        "vector_store_file",
        "discover",
        &[],
    )
    .await;
    assert_eq!(
        reserved.status, 200,
        "the current parent admits discovery: {reserved:?}"
    );
    assert_eq!(reserved.body["state"].as_str(), Some("reserved"));
    let observed = commit_relation(
        &f.dp,
        full_key,
        discover,
        json!([{"kind":"file","raw_id":raw_file}]),
    )
    .await;
    discovery_does_not_release_unreadable_child(&observed, raw_file);
}

// 创建父对象时的 Full 不可成为降为 Self 后读取另一成员既有文件的永久授权。
#[tokio::test]
async fn discovery_of_an_unrelated_existing_child_rechecks_a_full_to_self_downgrade() {
    let Some(f) = fixture().await else { return };
    let raw_file = "file_discovery_other_member_v14";
    let file = bind_root(&f.dp, &root_submission(&f, "file"), raw_file).await;
    let full_member = f.cp.seed_member(f.tenant, MemberRole::Admin).await.id;
    let full_key = Uuid::parse_str(&seed_key(&f.cp, &f.admin, f.tenant, Some(full_member)).await)
        .expect("the Full collection owner's key");
    let raw_store = "vs_created_while_full_v14";
    let mut store_submission = root_submission(&f, "vector_store");
    store_submission["api_key_id"] = json!(full_key);
    let store = bind_root(&f.dp, &store_submission, raw_store).await;
    let visible = lookup(&f.dp, full_key, "file", raw_file).await;
    assert_eq!(
        visible.status, 200,
        "Full can initially read the other member's file: {visible:?}"
    );
    assert_eq!(visible.body["record"]["record_id"], file["record_id"]);
    no_membership(&related(&f.dp, full_key, &store, "vector_store_file", raw_file).await);
    let owner = f.cp.member_login_seeded(f.tenant, MemberRole::Owner).await;
    let demoted =
        f.cp.send(
            "PATCH",
            &format!("/api/members/{full_member}"),
            Some(owner.cookie()),
            Some(json!({"role":"member"})),
        )
        .await;
    assert_eq!(
        demoted.status, 200,
        "the role downgrade is committed through the real API: {demoted:?}"
    );
    let denied = lookup(&f.dp, full_key, "file", raw_file).await;
    assert_eq!(
        denied.status, 404,
        "Self cannot read the other member's file: {denied:?}"
    );
    let still_owned = lookup(&f.dp, f.key, "file", raw_file).await;
    assert_eq!(
        still_owned.status, 200,
        "the file remains readable to its actual owner: {still_owned:?}"
    );
    assert_eq!(still_owned.body["record"]["record_id"], file["record_id"]);
    no_membership(&related(&f.dp, full_key, &store, "vector_store_file", raw_file).await);
    let parent = lookup(&f.dp, full_key, "vector_store", raw_store).await;
    assert_eq!(
        parent.status, 200,
        "Self still owns the original collection: {parent:?}"
    );
    let discover = Uuid::new_v4();
    let reserved = reserve_relation(
        &f.dp,
        full_key,
        discover,
        &parent.body["record"],
        "vector_store_file",
        "discover",
        &[],
    )
    .await;
    assert_eq!(
        reserved.status, 200,
        "the current owner may discover the collection: {reserved:?}"
    );
    assert_eq!(reserved.body["state"].as_str(), Some("reserved"));
    let observed = commit_relation(
        &f.dp,
        full_key,
        discover,
        json!([{"kind":"file","raw_id":raw_file}]),
    )
    .await;
    discovery_does_not_release_unreadable_child(&observed, raw_file);
}

// 撤权不抹掉已接受的逐项产物；过期 worker 和部分失败都不能留下半份归属或账单。
#[tokio::test]
async fn reclaimed_batch_lease_atomically_binds_each_entries_artifacts_after_key_revocation() {
    let Some(f) = fixture().await else { return };
    let second_model = f.cp.send("POST", "/admin/resources/models", Some(f.admin.cookie()), Some(json!({"doc":{
        "display_name":"foundation-second-model-v12","provider":"openai","model_name":"second-upstream-v12",
        "provider_key_id":f.submission["origin"]["provider_key_id"]
    }}))).await;
    assert_eq!(second_model.status, 201, "{second_model:?}");
    let second_model = uuid_at(&second_model.body, "/id");
    let first_model = uuid_at(&f.submission, "/origin/model_id");
    grant(&f.cp, &f.admin, f.tenant, &[first_model, second_model]).await;
    let current_reader =
        Uuid::parse_str(&seed_key(&f.cp, &f.admin, f.tenant, Some(f.member)).await)
            .expect("a live key for the same original member");

    let foreign_tenant = seed_tenant(&f.cp, &f.admin).await;
    grant(&f.cp, &f.admin, foreign_tenant, &[second_model]).await;
    let foreign_key = Uuid::parse_str(&seed_key(&f.cp, &f.admin, foreign_tenant, None).await)
        .expect("a foreign tenant's key");
    let mut foreign_submission = root_submission(&f, "response");
    foreign_submission["api_key_id"] = json!(foreign_key);
    foreign_submission["origin"]["model_id"] = json!(second_model);
    foreign_submission["requested_model_id"] = json!(second_model);
    foreign_submission["requested_model"] = json!("foundation-second-model-v12");
    foreign_submission["upstream_model"] = json!("second-upstream-v12");
    let foreign = bind_root(&f.dp, &foreign_submission, "resp_entry_conflict_v12").await;

    let mut submission = root_submission(&f, "message_batch");
    submission["asynchronous"] = json!(true);
    submission["entries"] = json!([
        {"entry_id":"one","endpoint":"/v1/responses","requested_model_id":first_model,
            "model_id":first_model,"upstream_model":"gpt-4o-mini"},
        {"entry_id":"two","endpoint":"/v1/responses","requested_model_id":second_model,
            "model_id":second_model,"upstream_model":"second-upstream-v12"}
    ]);
    let batch = bind_root(&f.dp, &submission, "batch_artifacts_exact_v12").await;
    let batch_id = uuid_at(&batch, "/record_id");
    let claim = json!({"limit":1,"record_id":batch_id});
    let original =
        f.dp.send("POST", "/dp/v1/native-objects/claim", Some(claim.clone()))
            .await;
    assert_eq!(original.status, 200, "{original:?}");
    let original_generation = original.body["records"][0]["lease_generation"]
        .as_i64()
        .expect("the original lease generation");
    sqlx::query("UPDATE native_objects SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(batch_id).execute(f.cp.db().pool()).await.expect("expire the crashed worker's lease");
    let key_version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id=$1")
        .bind(f.key)
        .fetch_one(f.cp.db().pool())
        .await
        .expect("the original key version");
    let revoked =
        f.cp.send_conditional(
            "DELETE",
            &format!("/admin/keys/{}", f.key),
            f.admin.cookie(),
            key_version,
            Value::Null,
        )
        .await;
    assert_eq!(revoked.status, 204, "{revoked:?}");
    let restarted = f.cp.restart().await;
    let recovery = restarted
        .issue_dp_client("foundation-artifact-recovery-v12")
        .await;
    let reclaimed = recovery
        .send("POST", "/dp/v1/native-objects/claim", Some(claim))
        .await;
    assert_eq!(reclaimed.status, 200, "{reclaimed:?}");
    let claims = reclaimed.body["records"]
        .as_array()
        .expect("explicit recovery claims");
    assert_eq!(claims.len(), 1);
    let generation = claims[0]["lease_generation"]
        .as_i64()
        .expect("the new lease generation");
    assert!(generation > original_generation);

    let observed = |entry: &str| {
        json!({
            "entry_id":entry,"root":{"kind":"response","raw_id":format!("resp_entry_{entry}_v12")},
            "objects":[
                {"kind":"response_item","raw_id":format!("msg_entry_{entry}_v12")},
                {"kind":"response_container","raw_id":format!("cntr_entry_{entry}_v12")}
            ],
            "relations":[
                {"parent":{"source":"root"},"child":{"source":"observed","kind":"response_item","raw_id":format!("msg_entry_{entry}_v12")},"relation":"response_output"},
                {"parent":{"source":"root"},"child":{"source":"observed","kind":"response_container","raw_id":format!("cntr_entry_{entry}_v12")},"relation":"response_output"}
            ]
        })
    };
    let lease = json!({"record_id":batch_id,"generation":generation});
    let completion = json!({
        "lease":lease,"terminal_status":"completed","usage":[
            {"entry_id":"one","occurred_at":chrono::Utc::now().to_rfc3339(),"status_code":200,
                "prompt_tokens":13,"completion_tokens":4,"total_tokens":17},
            {"entry_id":"two","occurred_at":chrono::Utc::now().to_rfc3339(),"status_code":200,
                "prompt_tokens":21,"completion_tokens":5,"total_tokens":26}
        ]
    });
    let payload =
        json!({"lease":lease,"entries":[observed("one"),observed("two")],"complete":completion});
    let mut stale = payload.clone();
    stale["lease"]["generation"] = json!(original_generation);
    stale["complete"]["lease"]["generation"] = json!(original_generation);
    let denied =
        f.dp.send(
            "POST",
            "/dp/v1/native-objects/observed-artifacts",
            Some(stale),
        )
        .await;
    assert_eq!(
        denied.status, 409,
        "old lease cannot publish accepted artifacts: {denied:?}"
    );

    let mut conflict = payload.clone();
    conflict["entries"][1]["root"]["raw_id"] = foreign["raw_id"].clone();
    let conflicted = recovery
        .send(
            "POST",
            "/dp/v1/native-objects/observed-artifacts",
            Some(conflict),
        )
        .await;
    assert_eq!(
        conflicted.status, 409,
        "a batch entry cannot claim another tenant's root: {conflicted:?}"
    );
    let partial: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM native_objects WHERE environment_id=$1 AND raw_id=ANY($2)",
    )
    .bind(restarted.env().env_id)
    .bind(vec![
        "resp_entry_one_v12",
        "msg_entry_one_v12",
        "cntr_entry_one_v12",
        "resp_entry_two_v12",
        "msg_entry_two_v12",
        "cntr_entry_two_v12",
    ])
    .fetch_one(restarted.db().pool())
    .await
    .expect("no artifacts from a rejected atomic observation");
    assert_eq!(
        partial, 0,
        "a conflict in the second entry rolls back the first entry too"
    );
    let before_outbox: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_terminal_outbox WHERE object_id=$1")
            .bind(batch_id)
            .fetch_one(restarted.db().pool())
            .await
            .expect("no terminal from a rejected observation");
    assert_eq!(before_outbox, 0);

    for _ in 0..2 {
        let bound = recovery
            .send(
                "POST",
                "/dp/v1/native-objects/observed-artifacts",
                Some(payload.clone()),
            )
            .await;
        assert_eq!(
            bound.status, 200,
            "accepted work binds after caller revocation: {bound:?}"
        );
        assert_eq!(bound.body["state"].as_str(), Some("observed"));
        assert_eq!(bound.body["record"]["record_id"], batch["record_id"]);
        let records = bound.body["records"]
            .as_array()
            .expect("all entry roots and artifacts");
        assert_eq!(records.len(), 6, "{bound:?}");
        let memberships = bound.body["memberships"]
            .as_array()
            .expect("entry output relations");
        assert_eq!(memberships.len(), 4, "{bound:?}");
        let mut roots = Vec::new();
        for (entry, model) in [("one", first_model), ("two", second_model)] {
            let root_raw = format!("resp_entry_{entry}_v12");
            let root = records
                .iter()
                .find(|record| record["raw_id"].as_str() == Some(root_raw.as_str()))
                .expect("each independent Response root");
            assert_eq!(root["kind"].as_str(), Some("response"));
            assert_eq!(root["origin"]["model_id"], json!(model));
            assert_eq!(root["requested_model_id"], json!(model));
            assert_eq!(root["api_key_id"], json!(f.key));
            roots.push(uuid_at(root, "/operation_id"));
            for (kind, prefix) in [("response_item", "msg"), ("response_container", "cntr")] {
                let raw = format!("{prefix}_entry_{entry}_v12");
                let child = records
                    .iter()
                    .find(|record| record["raw_id"].as_str() == Some(raw.as_str()))
                    .expect("every observed child is acknowledged");
                assert_eq!(child["kind"].as_str(), Some(kind));
                assert_eq!(child["api_key_id"], json!(f.key));
                assert_eq!(child["origin"]["model_id"], json!(model));
                assert_eq!(child["parent_id"], root["record_id"]);
                assert_eq!(
                    memberships
                        .iter()
                        .filter(
                            |membership| membership["parent_record_id"] == root["record_id"]
                                && membership["child_record_id"] == child["record_id"]
                                && membership["relation"].as_str() == Some("response_output")
                        )
                        .count(),
                    1
                );
                let revoked_read = lookup(&recovery, f.key, kind, &raw).await;
                assert_eq!(
                    revoked_read.status, 404,
                    "saved artifacts do not restore the revoked caller: {revoked_read:?}"
                );
                let live_read = lookup(&recovery, current_reader, kind, &raw).await;
                assert_eq!(
                    live_read.status, 200,
                    "the still-authorized original member owns its result: {live_read:?}"
                );
                assert_eq!(
                    live_read.body["record"]["raw_id"].as_str(),
                    Some(raw.as_str())
                );
            }
        }
        assert_ne!(
            roots[0], roots[1],
            "each batch entry needs an independent root operation"
        );
        assert!(roots
            .iter()
            .all(|root| *root != uuid_at(&submission, "/operation_id")));
    }
    let events: Vec<(String, Value)> = sqlx::query_as(
        "SELECT entry_id,event FROM native_terminal_outbox WHERE object_id=$1 ORDER BY entry_id",
    )
    .bind(batch_id)
    .fetch_all(restarted.db().pool())
    .await
    .expect("terminal and artifacts committed together");
    assert_eq!(
        events.len(),
        2,
        "replayed observation cannot duplicate settlement"
    );
    for ((entry, event), (expected_entry, model, input, output, total)) in events.iter().zip([
        ("one", first_model, 13, 4, 17),
        ("two", second_model, 21, 5, 26),
    ]) {
        assert_eq!(entry, expected_entry);
        assert_eq!(event["api_key_id"], json!(f.key));
        assert_eq!(event["requested_model_id"], json!(model));
        assert_eq!(event["prompt_tokens"], input);
        assert_eq!(event["completion_tokens"], output);
        assert_eq!(event["total_tokens"], total);
        assert_eq!(event["inference_stage"].as_str(), Some("terminal"));
    }
    let originals: i64 = sqlx::query_scalar("SELECT count(*) FROM native_objects WHERE environment_id=$1 AND raw_id='resp_entry_conflict_v12' AND api_key_id=$2")
        .bind(restarted.env().env_id).bind(foreign_key).fetch_one(restarted.db().pool()).await.expect("the conflicting root retains its original owner");
    assert_eq!(originals, 1);
}

fn training_usage(tokens: u64) -> Value {
    json!([{
        "entry_id":"", "upstream_model":"gpt-4o-mini",
        "occurred_at":"2026-11-14T01:02:03Z",
        "meter":{"kind":"training_tokens","trained_tokens":tokens}
    }])
}

async fn claimed_target(f: &Fixture, kind: &str, raw: &str) -> (Value, Value, Value) {
    let mut submission = root_submission(f, kind);
    submission["asynchronous"] = json!(true);
    let record = bind_root(&f.dp, &submission, raw).await;
    let claimed =
        f.dp.send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({
                "limit":1,"record_id":record["record_id"]
            })),
        )
        .await;
    assert_eq!(claimed.status, 200, "{claimed:?}");
    let records = claimed.body["records"]
        .as_array()
        .expect("explicit lease records");
    assert_eq!(records.len(), 1);
    let generation = records[0]["lease_generation"]
        .as_i64()
        .expect("an explicit generation");
    (
        submission,
        record.clone(),
        json!({"target":"object","lease":{
            "record_id":record["record_id"],"generation":generation
        }}),
    )
}

async fn typed_complete(dp: &DpClient, target: &Value, usage: Value) -> Resp {
    dp.send(
        "POST",
        "/dp/v1/native-objects/complete-typed",
        Some(json!({
            "target":target,"terminal_status":"completed","usage":usage
        })),
    )
    .await
}

fn awaiting_unpriced(reply: &Resp, operation: Uuid) {
    assert_eq!(reply.status, 200, "{reply:?}");
    assert_eq!(reply.body["state"].as_str(), Some("reconciliation"));
    assert_eq!(reply.body["operation_id"], json!(operation));
    assert_eq!(
        reply.body["metering_state"].as_str(),
        Some("awaiting_reconciliation")
    );
    assert_eq!(
        reply.body["reconciliation_reason"].as_str(),
        Some("unpriced_meter")
    );
}

async fn outbox_count(cp: &TestCp, record: &Value) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM native_terminal_outbox WHERE object_id=$1")
        .bind(uuid_at(record, "/record_id"))
        .fetch_one(cp.db().pool())
        .await
        .expect("the exact object's terminal count")
}

// reported 0 是真实的已知量；它和 3 都不能在没有训练费率合同的时候伪造 token 免费终态。
#[tokio::test]
async fn reported_training_quantities_including_zero_wait_for_pricing_without_token_settlement() {
    let Some(f) = fixture().await else { return };
    for tokens in [3_u64, 0] {
        let raw = format!("ftjob_training_report_{tokens}_v12");
        let (submission, record, target) = claimed_target(&f, "fine_tuning_job", &raw).await;
        let usage = training_usage(tokens);
        let reply = typed_complete(&f.dp, &target, usage.clone()).await;
        awaiting_unpriced(&reply, uuid_at(&submission, "/operation_id"));
        let stored: (String, Option<String>, Value, i64) = sqlx::query_as(
            "SELECT state,reason,observed_usage,receipt_version FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
            .bind(f.cp.env().env_id).bind(uuid_at(&submission,"/operation_id"))
            .fetch_one(f.cp.db().pool()).await.expect("the observed training receipt");
        assert_eq!(stored.0, "awaiting_reconciliation");
        assert_eq!(stored.1.as_deref(), Some("unpriced_meter"));
        assert_eq!(stored.2, usage);
        assert_eq!(
            stored.2[0]["meter"]["trained_tokens"].as_u64(),
            Some(tokens)
        );
        assert_eq!(stored.3, 1);
        let object: (
            bool,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<String>,
            i64,
        ) = sqlx::query_as(
            "SELECT pending,completed_at,terminal_status,version FROM native_objects WHERE id=$1",
        )
        .bind(uuid_at(&record, "/record_id"))
        .fetch_one(f.cp.db().pool())
        .await
        .expect("the completed upstream object with unsettled pricing");
        assert!(
            !object.0,
            "the complete supplier report ended the upstream lifecycle"
        );
        assert!(
            object.1.is_some(),
            "known terminal quantities complete the upstream object"
        );
        assert_eq!(object.2.as_deref(), Some("completed"));
        assert_eq!(
            object.3,
            record["version"]
                .as_i64()
                .expect("the original object version")
                + 1
        );
        assert_eq!(outbox_count(&f.cp, &record).await, 0);
        let readable = lookup(&f.dp, f.key, "fine_tuning_job", &raw).await;
        assert_eq!(readable.status, 200, "{readable:?}");
        assert_eq!(readable.body["record"]["completed"].as_bool(), Some(true));
        let deleted = f.dp.send("POST", "/dp/v1/native-objects/tombstone", Some(json!({
            "api_key_id":f.key,"record_id":record["record_id"],"version":readable.body["record"]["version"]
        }))).await;
        assert_eq!(deleted.status, 200, "completed upstream objects may be removed while pricing awaits reconciliation: {deleted:?}");
        let retained: (String,Value) = sqlx::query_as("SELECT state,observed_usage FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
            .bind(f.cp.env().env_id).bind(uuid_at(&submission,"/operation_id")).fetch_one(f.cp.db().pool()).await.expect("deleting the object cannot discard its ledger facts");
        assert_eq!(retained, ("awaiting_reconciliation".into(), usage));
    }
}

// 先收到的缺量事实必须保留；真实补量只能追加新版，不能把旧观察改写成 reported 0。
#[tokio::test]
async fn missing_usage_remains_unknown_and_training_reconciliation_appends_immutable_history() {
    let Some(f) = fixture().await else { return };
    let (submission, record, target) =
        claimed_target(&f, "fine_tuning_job", "ftjob_missing_then_reported_v12").await;
    let operation = uuid_at(&submission, "/operation_id");
    let unknown = f.dp.send("POST", "/dp/v1/native-objects/reconciliation", Some(json!({
        "target":target,"terminal_status":"completed","reason":"missing_usage","observed_usage":[]
    }))).await;
    assert_eq!(unknown.status, 200, "{unknown:?}");
    assert_eq!(unknown.body["state"].as_str(), Some("reconciliation"));
    assert_eq!(unknown.body["metering_state"].as_str(), Some("unknown"));
    assert_eq!(
        unknown.body["reconciliation_reason"].as_str(),
        Some("missing_usage")
    );
    let first: (i64, String, Value) = sqlx::query_as(
        "SELECT receipt_version,state,observed_usage FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("the explicit missing observation");
    assert_eq!((first.0, first.1.as_str()), (1, "unknown"));
    assert!(first
        .2
        .as_array()
        .expect("explicit unknown quantities")
        .is_empty());
    let original: Value = sqlx::query_scalar(
        "SELECT receipt FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2 AND version=1")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("version one of the receipt");
    assert_eq!(outbox_count(&f.cp, &record).await, 0);
    let recoverable: (bool, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT pending,completed_at FROM native_objects WHERE id=$1")
            .bind(uuid_at(&record, "/record_id"))
            .fetch_one(f.cp.db().pool())
            .await
            .expect("the missing-usage object remains recoverable");
    assert!(
        recoverable.0,
        "missing usage cannot finish the observation lifecycle"
    );
    assert!(recoverable.1.is_none());
    let usage = training_usage(3);
    let supplemented = typed_complete(&f.dp, &target, usage.clone()).await;
    awaiting_unpriced(&supplemented, operation);
    let rows: Vec<(i64, Value)> = sqlx::query_as(
        "SELECT version,receipt FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2 ORDER BY version")
        .bind(f.cp.env().env_id).bind(operation).fetch_all(f.cp.db().pool()).await.expect("the immutable receipt history");
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].0, rows[1].0), (1, 2));
    assert_eq!(rows[0].1, original);
    let latest: (i64,Value) = sqlx::query_as(
        "SELECT receipt_version,observed_usage FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("the appended real training quantity");
    assert_eq!(latest, (2, usage));
    let rewrite = sqlx::query("UPDATE native_metering_receipts SET receipt=receipt || '{\"rewritten\":true}'::jsonb WHERE environment_id=$1 AND operation_id=$2 AND version=1")
        .bind(f.cp.env().env_id).bind(operation).execute(f.cp.db().pool()).await;
    assert!(
        rewrite.is_err(),
        "an existing receipt cannot be rewritten even before settlement"
    );
    assert_eq!(outbox_count(&f.cp, &record).await, 0);
}

// 有效租约承接已接受工作；旧代、另一 DP 和原调用方撤权分别约束不同的权限。
#[tokio::test]
async fn typed_object_receipts_reject_stale_and_wrong_dp_but_survive_caller_revocation() {
    let Some(f) = fixture().await else { return };
    let (submission, record, old_target) =
        claimed_target(&f, "fine_tuning_job", "ftjob_reclaimed_meter_v12").await;
    let bypass =
        json!({"target":"operation","operation_id":submission["operation_id"],"api_key_id":f.key});
    let complete_bypass = typed_complete(&f.dp, &bypass, training_usage(3)).await;
    assert_eq!(
        complete_bypass.status, 422,
        "a bound asynchronous object requires its lease: {complete_bypass:?}"
    );
    let reconcile_bypass =
        f.dp.send(
            "POST",
            "/dp/v1/native-objects/reconciliation",
            Some(json!({
                "target":bypass,"reason":"uncertain_submission"
            })),
        )
        .await;
    assert_eq!(reconcile_bypass.status,422,"reconciliation cannot reserve a bound object's receipt without its lease: {reconcile_bypass:?}");
    let before_receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(f.cp.env().env_id)
    .bind(uuid_at(&submission, "/operation_id"))
    .fetch_one(f.cp.db().pool())
    .await
    .expect("no operation-target receipt can poison the bound object");
    assert_eq!(before_receipts, 0);
    sqlx::query("UPDATE native_objects SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(uuid_at(&record,"/record_id")).execute(f.cp.db().pool()).await.expect("expire the abandoned observer lease");
    let recovery =
        f.cp.issue_dp_client("foundation-typed-meter-recovery-v12")
            .await;
    let claim = recovery
        .send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({"limit":1,"record_id":record["record_id"]})),
        )
        .await;
    assert_eq!(claim.status, 200, "{claim:?}");
    let generation = claim.body["records"][0]["lease_generation"]
        .as_i64()
        .expect("the reclaimed lease generation");
    assert!(
        generation
            > old_target["lease"]["generation"]
                .as_i64()
                .expect("old lease generation")
    );
    let target = json!({"target":"object","lease":{"record_id":record["record_id"],"generation":generation}});
    let stale = typed_complete(&f.dp, &old_target, training_usage(3)).await;
    assert_eq!(
        stale.status, 409,
        "an old lease cannot append a receipt: {stale:?}"
    );
    let wrong_dp = typed_complete(&f.dp, &target, training_usage(3)).await;
    assert_eq!(
        wrong_dp.status, 409,
        "only the reclaimed lease's DP can observe: {wrong_dp:?}"
    );
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id=$1")
        .bind(f.key)
        .fetch_one(f.cp.db().pool())
        .await
        .expect("the original caller version");
    let revoked =
        f.cp.send_conditional(
            "DELETE",
            &format!("/admin/keys/{}", f.key),
            f.admin.cookie(),
            version,
            Value::Null,
        )
        .await;
    assert_eq!(revoked.status, 204, "{revoked:?}");
    let accepted = typed_complete(&recovery, &target, training_usage(3)).await;
    awaiting_unpriced(&accepted, uuid_at(&submission, "/operation_id"));
    let denied = lookup(
        &recovery,
        f.key,
        "fine_tuning_job",
        "ftjob_reclaimed_meter_v12",
    )
    .await;
    assert_eq!(
        denied.status, 404,
        "an accepted receipt does not restore caller access: {denied:?}"
    );
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(f.cp.env().env_id)
    .bind(uuid_at(&submission, "/operation_id"))
    .fetch_one(f.cp.db().pool())
    .await
    .expect("only the valid observer creates a receipt");
    assert_eq!(receipts, 1);
    assert_eq!(outbox_count(&f.cp, &record).await, 0);
}

// 同步操作可以没有上游对象；只有原始受理的 DP 和 key 组合能写这个事实。
#[tokio::test]
async fn operation_metering_pins_original_dp_and_key_without_fabricating_an_object() {
    let Some(f) = fixture().await else { return };
    let submission = root_submission(&f, "fine_tuning_job");
    let begun =
        f.dp.send(
            "POST",
            "/dp/v1/native-objects/begin",
            Some(submission.clone()),
        )
        .await;
    assert_eq!(begun.status, 200, "{begun:?}");
    assert_eq!(begun.body["state"].as_str(), Some("pending_submission"));
    let operation = uuid_at(&submission, "/operation_id");
    let target = json!({"target":"operation","operation_id":operation,"api_key_id":f.key});
    let other_dp =
        f.cp.issue_dp_client("foundation-unrelated-operation-observer-v12")
            .await;
    let wrong_dp = typed_complete(&other_dp, &target, training_usage(3)).await;
    assert_eq!(wrong_dp.status, 404, "{wrong_dp:?}");
    let other_key = Uuid::parse_str(&seed_key(&f.cp, &f.admin, f.tenant, Some(f.member)).await)
        .expect("another live key");
    let mut wrong_target = target.clone();
    wrong_target["api_key_id"] = json!(other_key);
    let wrong_key = typed_complete(&f.dp, &wrong_target, training_usage(3)).await;
    assert_eq!(
        wrong_key.status, 404,
        "another current key cannot replace the accepted caller: {wrong_key:?}"
    );
    let accepted = typed_complete(&f.dp, &target, training_usage(3)).await;
    awaiting_unpriced(&accepted, operation);
    assert!(
        accepted.body.get("record").is_none_or(Value::is_null),
        "no provider object was observed: {accepted:?}"
    );
    let objects: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM native_objects WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(f.cp.env().env_id)
    .bind(operation)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("operation-only observation creates no object");
    assert_eq!(objects, 0);
    let record: (Option<Uuid>,String,Uuid,i64) = sqlx::query_as("SELECT object_id,target_kind,api_key_id,receipt_version FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("the operation-scoped metering identity");
    assert_eq!(record, (None, "operation".into(), f.key, 1));

    let mut asynchronous = root_submission(&f, "fine_tuning_job");
    asynchronous["asynchronous"] = json!(true);
    let begun =
        f.dp.send(
            "POST",
            "/dp/v1/native-objects/begin",
            Some(asynchronous.clone()),
        )
        .await;
    assert_eq!(begun.status, 200, "{begun:?}");
    let operation = uuid_at(&asynchronous, "/operation_id");
    let operation_target =
        json!({"target":"operation","operation_id":operation,"api_key_id":f.key});
    let premature = typed_complete(&f.dp, &operation_target, training_usage(3)).await;
    assert_eq!(
        premature.status, 422,
        "an asynchronous submission without a bound object has no terminal lease: {premature:?}"
    );
    let missing =
        f.dp.send(
            "POST",
            "/dp/v1/native-objects/reconciliation",
            Some(json!({
                "target":operation_target,"reason":"missing_usage"
            })),
        )
        .await;
    assert_eq!(
        missing.status, 422,
        "only submission uncertainty can precede an asynchronous object binding: {missing:?}"
    );
    let uncertain =
        f.dp.send(
            "POST",
            "/dp/v1/native-objects/reconciliation",
            Some(json!({
                "target":operation_target,"reason":"uncertain_submission"
            })),
        )
        .await;
    assert_eq!(uncertain.status, 200, "{uncertain:?}");
    assert_eq!(uncertain.body["metering_state"].as_str(), Some("unknown"));
    let first: Value = sqlx::query_scalar("SELECT receipt FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2 AND version=1")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("the original unbound submission uncertainty receipt");
    let bound = f.dp.send("POST","/dp/v1/native-objects/bind",Some(json!({
        "operation_id":operation,"api_key_id":f.key,"raw_id":"ftjob_uncertain_then_bound_v12"
    }))).await;
    assert_eq!(
        bound.status, 200,
        "the same accepted operation can acquire its observed raw identity: {bound:?}"
    );
    let object_id = uuid_at(&bound.body, "/record/record_id");
    let claimed =
        f.dp.send(
            "POST",
            "/dp/v1/native-objects/claim",
            Some(json!({"limit":1,"record_id":object_id})),
        )
        .await;
    assert_eq!(claimed.status, 200, "{claimed:?}");
    let generation = claimed.body["records"][0]["lease_generation"]
        .as_i64()
        .expect("the observed asynchronous object's lease");
    let object_target =
        json!({"target":"object","lease":{"record_id":object_id,"generation":generation}});
    awaiting_unpriced(
        &typed_complete(&f.dp, &object_target, training_usage(3)).await,
        operation,
    );
    let upgraded: (Option<Uuid>,String,i64) = sqlx::query_as("SELECT object_id,target_kind,receipt_version FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("the receipt follows the same operation into its observed object identity");
    assert_eq!(upgraded, (Some(object_id), "object".into(), 2));
    let history: Vec<(i64,Value)> = sqlx::query_as("SELECT version,receipt FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2 ORDER BY version")
        .bind(f.cp.env().env_id).bind(operation).fetch_all(f.cp.db().pool()).await.expect("both uncertainty and actual observation are retained");
    assert_eq!(history.len(), 2);
    assert_eq!((history[0].0, history[1].0), (1, 2));
    assert_eq!(history[0].1, first);
}

fn token_usage(entry: &str) -> Value {
    json!([{
        "entry_id":entry,"upstream_model":"gpt-4o-mini","occurred_at":"2026-11-14T01:02:03Z",
        "meter":{"kind":"tokens","usage":{
            "entry_id":entry,"occurred_at":"2026-11-14T01:02:03Z","status_code":200,
            "prompt_tokens":40,"completion_tokens":10,"total_tokens":50,
            "cached_prompt_tokens":0,"cache_usage_reported":true,
            "error_class":"operator_retained_detail","error_message":"private-metering-error-sentinel-v12",
            "io_audit":{"version":2,"boundary":"upstream_attempt","response_complete":true,
                "exchange_count":1,"input_text":{"utf8_bytes":5,"unicode_scalars":5,"complete":true},
                "output_text":{"utf8_bytes":2,"unicode_scalars":2,"complete":true}}
        }}
    }])
}

async fn wait_for_metering_state(
    cp: &TestCp,
    operation: Uuid,
    expected: &str,
) -> (String, Option<String>, i64) {
    tokio::time::timeout(std::time::Duration::from_secs(15),async {
        loop {
            let stored: (String,Option<String>,i64) = sqlx::query_as(
                "SELECT state,reason,receipt_version FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
                .bind(cp.env().env_id).bind(operation).fetch_one(cp.db().pool()).await.expect("the durable delivery state");
            if stored.0==expected { return stored; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.unwrap_or_else(|_|panic!("real outbox delivery did not move {operation} to {expected}"))
}

// typed token 终态必须继续走已有 outbox；另一个 entry 或上游模型不能借此冒充接受清单。
#[tokio::test]
async fn typed_token_receipts_require_exact_entries_and_keep_the_existing_outbox_once() {
    let Some(f) = fixture().await else { return };
    let (submission, record, target) =
        claimed_target(&f, "response", "resp_typed_tokens_exact_v12").await;
    let wrong_entry = typed_complete(&f.dp, &target, token_usage("unaccepted-entry-v12")).await;
    assert_eq!(
        wrong_entry.status, 422,
        "token entry must belong to the accepted manifest: {wrong_entry:?}"
    );
    let mut wrong_model = token_usage("");
    wrong_model[0]["upstream_model"] = json!("unaccepted-upstream-model-v12");
    let wrong_model = typed_complete(&f.dp, &target, wrong_model).await;
    assert_eq!(
        wrong_model.status, 422,
        "token model must match the accepted entry: {wrong_model:?}"
    );
    assert_eq!(outbox_count(&f.cp, &record).await, 0);
    let complete = typed_complete(&f.dp, &target, token_usage("")).await;
    assert_eq!(complete.status, 200, "{complete:?}");
    assert_eq!(complete.body["state"].as_str(), Some("completed"));
    assert_eq!(
        complete.body["metering_state"].as_str(),
        Some("outbox_pending")
    );
    assert!(complete
        .body
        .get("reconciliation_reason")
        .is_none_or(Value::is_null));
    assert_eq!(outbox_count(&f.cp, &record).await, 1);
    let event: Value =
        sqlx::query_scalar("SELECT event FROM native_terminal_outbox WHERE object_id=$1")
            .bind(uuid_at(&record, "/record_id"))
            .fetch_one(f.cp.db().pool())
            .await
            .expect("the single ordinary token terminal");
    assert_eq!(event["prompt_tokens"].as_u64(), Some(40));
    assert_eq!(event["completion_tokens"].as_u64(), Some(10));
    assert_eq!(event["total_tokens"].as_u64(), Some(50));
    assert_eq!(event["cached_prompt_tokens"].as_u64(), Some(0));
    assert_eq!(event["api_key_id"], json!(f.key));
    assert_eq!(event["native_operation_id"], submission["operation_id"]);
    let operation = uuid_at(&submission, "/operation_id");
    let accepted_receipt: (String,Value) = sqlx::query_as("SELECT state,receipt FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2 AND version=1")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("the immutable pre-delivery receipt");
    assert_eq!(accepted_receipt.0, "outbox_pending");
    let receipt = wait_for_metering_state(&f.cp, operation, "awaiting_reconciliation").await;
    assert_eq!(
        receipt,
        (
            "awaiting_reconciliation".into(),
            Some("unpriced_meter".into()),
            2
        )
    );
    let accounting: (bool,String) = sqlx::query_as("SELECT o.delivered_at IS NOT NULL,c.status FROM native_terminal_outbox o JOIN pricing_charges_cny c ON c.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(uuid_at(&record,"/record_id")).fetch_one(f.cp.db().pool()).await.expect("the actual unpriced billing intake");
    assert_eq!(accounting, (true, "unpriced".into()));
    let history: Vec<(i64,String,Value)> = sqlx::query_as("SELECT version,state,receipt FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2 ORDER BY version")
        .bind(f.cp.env().env_id).bind(operation).fetch_all(f.cp.db().pool()).await.expect("acceptance and delivery are separate immutable receipts");
    assert_eq!(history.len(), 2);
    assert_eq!((history[0].0, history[0].1.as_str()), (1, "outbox_pending"));
    assert_eq!(
        (history[1].0, history[1].1.as_str()),
        (2, "awaiting_reconciliation")
    );
    assert_eq!(history[0].2, accepted_receipt.1);
    let replay = typed_complete(&f.dp, &target, token_usage("")).await;
    awaiting_unpriced(&replay, operation);
    let receipt: (String,i64) = sqlx::query_as("SELECT state,receipt_version FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("a replay retains the actual delivered ledger state");
    assert_eq!(receipt, ("awaiting_reconciliation".into(), 2));
    assert_eq!(outbox_count(&f.cp, &record).await, 1);
}

fn no_operator_receipt_secrets(value: &Value) {
    match value {
        Value::Object(fields) => {
            for (name, value) in fields {
                assert!(
                    !matches!(
                        name.as_str(),
                        "artifacts"
                            | "raw_usage"
                            | "raw_id"
                            | "boundary"
                            | "error_class"
                            | "error_message"
                            | "io_audit"
                    ),
                    "the operator quantity projection excludes {name}"
                );
                no_operator_receipt_secrets(value);
            }
        }
        Value::Array(values) => values.iter().for_each(no_operator_receipt_secrets),
        Value::String(text) => assert!(!matches!(
            text.as_str(),
            "private-metering-error-sentinel-v12" | "private-raw-provider-report-v12"
        )),
        _ => {}
    }
}

fn has_exact_receipt_field(value: &Value, name: &str, expected: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields.get(name) == Some(expected)
                || fields
                    .values()
                    .any(|value| has_exact_receipt_field(value, name, expected))
        }
        Value::Array(values) => values
            .iter()
            .any(|value| has_exact_receipt_field(value, name, expected)),
        _ => false,
    }
}

// Viewer 只能看计量投影；分页必须覆盖所有匹配 receipt，不带回 artifact、错误正文或原始供应商报告。
#[tokio::test]
async fn operator_metering_query_filters_and_pages_quantities_without_exposing_private_receipts() {
    let Some(f) = fixture().await else { return };
    let mut training_operations = std::collections::BTreeMap::new();
    for tokens in [3_u64, 0] {
        let raw = format!("ftjob_query_training_{tokens}_v12");
        let (submission, _, target) = claimed_target(&f, "fine_tuning_job", &raw).await;
        let operation = uuid_at(&submission, "/operation_id");
        awaiting_unpriced(
            &typed_complete(&f.dp, &target, training_usage(tokens)).await,
            operation,
        );
        training_operations.insert(operation, tokens);
    }
    let (token_submission, _, token_target) =
        claimed_target(&f, "response", "resp_query_tokens_v12").await;
    let token_complete = typed_complete(&f.dp, &token_target, token_usage("")).await;
    assert_eq!(token_complete.status, 200, "{token_complete:?}");
    assert_eq!(
        token_complete.body["metering_state"].as_str(),
        Some("outbox_pending")
    );
    wait_for_metering_state(
        &f.cp,
        uuid_at(&token_submission, "/operation_id"),
        "awaiting_reconciliation",
    )
    .await;
    let (missing_submission, _, missing_target) =
        claimed_target(&f, "fine_tuning_job", "ftjob_query_missing_v12").await;
    let missing = f.dp.send("POST","/dp/v1/native-objects/reconciliation",Some(json!({
        "target":missing_target,"terminal_status":"completed","reason":"missing_usage","observed_usage":[],
        "raw_usage":{"provider_report":"private-raw-provider-report-v12"}
    }))).await;
    assert_eq!(missing.status, 200, "{missing:?}");
    let raw_receipt: Value = sqlx::query_scalar("SELECT receipt FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2 AND version=1")
        .bind(f.cp.env().env_id).bind(uuid_at(&missing_submission,"/operation_id")).fetch_one(f.cp.db().pool()).await.expect("the original private supplier report remains in the authority receipt");
    assert!(
        has_exact_receipt_field(
            &raw_receipt,
            "raw_usage",
            &json!({"provider_report":"private-raw-provider-report-v12"})
        ),
        "private raw usage is preserved internally before it is filtered from the read projection"
    );
    let viewer = f.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let mut after = None;
    let mut seen = std::collections::BTreeMap::new();
    loop {
        let mut path = "/admin/native-metering?state=awaiting_reconciliation&reason=unpriced_meter&kind=fine_tuning_job&limit=1".to_string();
        if let Some(cursor) = after {
            path.push_str(&format!("&after={cursor}"));
        }
        let page = f.cp.send("GET", &path, Some(viewer.cookie()), None).await;
        assert_eq!(page.status, 200, "Viewer may inspect quantities: {page:?}");
        no_operator_receipt_secrets(&page.body);
        let items = page.body["items"]
            .as_array()
            .expect("an explicit metering page");
        assert_eq!(items.len(), 1, "limit and filters apply exactly: {page:?}");
        let item = &items[0];
        let operation = uuid_at(item, "/operation_id");
        let expected = *training_operations
            .get(&operation)
            .expect("the page contains only the two requested training receipts");
        assert!(
            seen.insert(operation, expected).is_none(),
            "no repeated receipt across pages"
        );
        assert_eq!(item["target_kind"].as_str(), Some("object"));
        assert_eq!(item["kind"].as_str(), Some("fine_tuning_job"));
        assert_eq!(item["protocol"].as_str(), Some("openai"));
        assert_eq!(
            item["metering_state"].as_str(),
            Some("awaiting_reconciliation")
        );
        assert_eq!(
            item["reconciliation_reason"].as_str(),
            Some("unpriced_meter")
        );
        assert_eq!(item["receipt_version"].as_i64(), Some(1));
        assert_eq!(item["owner"]["tenant_id"], json!(f.tenant));
        assert_eq!(item["owner"]["api_key_id"], json!(f.key));
        assert_eq!(item["owner"]["member_id"], json!(f.member));
        let quantities = item["quantities"]
            .as_array()
            .expect("explicit observed quantities");
        assert_eq!(quantities.len(), 1);
        assert_eq!(
            quantities[0]["meter"]["kind"].as_str(),
            Some("training_tokens")
        );
        assert_eq!(
            quantities[0]["meter"]["trained_tokens"].as_u64(),
            Some(expected)
        );
        for field in ["created_at", "updated_at"] {
            chrono::DateTime::parse_from_rfc3339(
                item[field].as_str().expect("explicit receipt timestamp"),
            )
            .expect("a valid receipt timestamp");
        }
        let more = page.body["has_more"]
            .as_bool()
            .expect("explicit pagination state");
        let cursor = page
            .body
            .get("next_after")
            .expect("explicit next cursor or null");
        if !more {
            assert!(cursor.is_null());
            break;
        }
        let cursor = Uuid::parse_str(cursor.as_str().expect("next operation cursor"))
            .expect("a UUID cursor");
        assert_ne!(after, Some(cursor), "pagination must make progress");
        after = Some(cursor);
        assert!(
            seen.len() < training_operations.len(),
            "has_more must describe a remaining visible receipt"
        );
    }
    assert_eq!(
        seen, training_operations,
        "pagination exhausts every matching receipt exactly once"
    );
    let tokens =
        f.cp.send(
            "GET",
            "/admin/native-metering?state=awaiting_reconciliation&reason=unpriced_meter&kind=response",
            Some(viewer.cookie()),
            None,
        )
        .await;
    assert_eq!(tokens.status, 200, "{tokens:?}");
    no_operator_receipt_secrets(&tokens.body);
    let items = tokens.body["items"]
        .as_array()
        .expect("the token meter page");
    assert_eq!(items.len(), 1);
    let meter = &items[0]["quantities"][0]["meter"];
    assert_eq!(meter["kind"].as_str(), Some("tokens"));
    assert_eq!(meter["prompt_tokens"].as_u64(), Some(40));
    assert_eq!(meter["completion_tokens"].as_u64(), Some(10));
    assert_eq!(meter["total_tokens"].as_u64(), Some(50));
    assert_eq!(meter["cached_prompt_tokens"].as_u64(), Some(0));
    assert!(
        meter.get("usage").is_none(),
        "the raw TerminalUsage is not an operator quantity DTO"
    );
    let unknown =
        f.cp.send(
            "GET",
            "/admin/native-metering?state=unknown&reason=missing_usage&kind=fine_tuning_job",
            Some(viewer.cookie()),
            None,
        )
        .await;
    assert_eq!(unknown.status, 200, "{unknown:?}");
    no_operator_receipt_secrets(&unknown.body);
    let items = unknown.body["items"]
        .as_array()
        .expect("the unknown metering page");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["operation_id"], missing_submission["operation_id"]);
    assert!(items[0]["quantities"]
        .as_array()
        .expect("explicit missing quantities")
        .is_empty());
    let member = f.cp.member_login_seeded(f.tenant, MemberRole::Owner).await;
    let tenant =
        f.cp.send("GET", "/admin/native-metering", Some(member.cookie()), None)
            .await;
    assert!(
        matches!(tenant.status, 401 | 403),
        "a portal tenant is not an operator: {tenant:?}"
    );
    assert!(tenant.body.get("items").is_none());
    let anonymous = f.cp.send("GET", "/admin/native-metering", None, None).await;
    assert_eq!(
        anonymous.status, 401,
        "metering receipts are not public: {anonymous:?}"
    );
}

// Eval 的接受清单是授权集合；未被真正调用的模型不能为了凑齐清单而生成零用量。
#[tokio::test]
async fn eval_run_settles_only_observed_accepted_models_including_canceled_runs() {
    let Some(f) = fixture().await else { return };
    let second = f.cp.send("POST", "/admin/resources/models", Some(f.admin.cookie()), Some(json!({"doc":{
        "display_name":"eval-authorized-second-v12","provider":"openai","model_name":"eval-second-upstream-v12",
        "provider_key_id":f.submission["origin"]["provider_key_id"]
    }}))).await;
    assert_eq!(second.status, 201, "{second:?}");
    let first_model = uuid_at(&f.submission, "/origin/model_id");
    let second_model = uuid_at(&second.body, "/id");
    grant(&f.cp, &f.admin, f.tenant, &[first_model, second_model]).await;
    let eval = bind_root(
        &f.dp,
        &root_submission(&f, "eval"),
        "eval_definition_exact_v12",
    )
    .await;
    for status in ["completed", "canceled"] {
        let mut submission = root_submission(&f, "eval_run");
        submission["parent_id"] = eval["record_id"].clone();
        submission["asynchronous"] = json!(true);
        submission["entries"] = json!([
            {"entry_id":"model-a","endpoint":"/v1/evals","requested_model_id":first_model,
                "model_id":first_model,"upstream_model":"gpt-4o-mini"},
            {"entry_id":"model-b","endpoint":"/v1/evals","requested_model_id":second_model,
                "model_id":second_model,"upstream_model":"eval-second-upstream-v12"}
        ]);
        let raw = format!("evalrun_subset_{status}_v12");
        let run = bind_root(&f.dp, &submission, &raw).await;
        let claimed =
            f.dp.send(
                "POST",
                "/dp/v1/native-objects/claim",
                Some(json!({
                    "limit":1,"record_id":run["record_id"]
                })),
            )
            .await;
        assert_eq!(claimed.status, 200, "{claimed:?}");
        let generation = claimed.body["records"][0]["lease_generation"]
            .as_i64()
            .expect("the EvalRun observer lease");
        let target = json!({"target":"object","lease":{"record_id":run["record_id"],"generation":generation}});
        let payload = |usage| json!({"target":target,"terminal_status":status,"usage":usage});
        let actual = token_usage("model-a");
        assert!(actual[0]["meter"]["usage"]
            .get("cache_write_tokens")
            .is_none());
        assert_eq!(
            actual[0]["meter"]["usage"]["cached_prompt_tokens"].as_u64(),
            Some(0)
        );
        // 持久层存 typed DTO 的规范值；缺失的可选量是 null，真实报告的缓存零量仍为 0。
        // 期望手写公开契约的完整形状，不调用产品 serializer 推导断言。
        let canonical_io_audit = json!({
            "version":2,"boundary":"upstream_attempt","response_complete":true,
            "exchange_count":1,"input_text":{"utf8_bytes":5,"unicode_scalars":5,"complete":true},
            "output_text":{"utf8_bytes":2,"unicode_scalars":2,"complete":true}
        });
        let canonical_usage = json!({
            "entry_id":"model-a", "occurred_at":"2026-11-14T01:02:03Z", "status_code":200,
            "prompt_tokens":40, "completion_tokens":10, "total_tokens":50,
            "cached_prompt_tokens":0, "cache_usage_reported":true,
            "cache_write_tokens":null, "cache_creation_tokens":0, "cache_read_tokens":0,
            "cache_creation_5m_tokens":null, "cache_creation_1h_tokens":null, "reasoning_tokens":0,
            "prompt_audio_tokens":null, "prompt_image_tokens":null,
            "completion_audio_tokens":null, "completion_image_tokens":null,
            "web_search_requests":0, "usage_estimated":false,
            "service_tier":null, "requested_service_tier":null,
            "error_class":"operator_retained_detail", "error_message":"private-metering-error-sentinel-v12",
            "io_audit":canonical_io_audit
        });
        let canonical_observation = json!({
            "entry_id":"model-a", "upstream_model":"gpt-4o-mini", "occurred_at":"2026-11-14T01:02:03Z",
            "meter":{"kind":"tokens", "usage":canonical_usage}
        });
        let canonical_observed = json!([canonical_observation]);
        let unknown =
            f.dp.send(
                "POST",
                "/dp/v1/native-objects/complete-typed",
                Some(payload(token_usage("unaccepted-eval-model"))),
            )
            .await;
        assert_eq!(
            unknown.status, 422,
            "an Eval cannot settle an unaccepted model: {unknown:?}"
        );
        let mut wrong_model = actual.clone();
        wrong_model[0]["upstream_model"] = json!("eval-second-upstream-v12");
        let wrong_model =
            f.dp.send(
                "POST",
                "/dp/v1/native-objects/complete-typed",
                Some(payload(wrong_model)),
            )
            .await;
        assert_eq!(
            wrong_model.status, 422,
            "each actual entry retains its accepted model: {wrong_model:?}"
        );
        let duplicate = json!([actual[0].clone(), actual[0].clone()]);
        let duplicate =
            f.dp.send(
                "POST",
                "/dp/v1/native-objects/complete-typed",
                Some(payload(duplicate)),
            )
            .await;
        assert_eq!(
            duplicate.status, 422,
            "duplicate per-model reports cannot double bill: {duplicate:?}"
        );
        assert_eq!(outbox_count(&f.cp, &run).await, 0);
        let completed =
            f.dp.send(
                "POST",
                "/dp/v1/native-objects/complete-typed",
                Some(payload(actual.clone())),
            )
            .await;
        assert_eq!(
            completed.status, 200,
            "a nonempty observed subset is sufficient for {status}: {completed:?}"
        );
        assert_eq!(completed.body["state"].as_str(), Some("completed"));
        assert_eq!(
            completed.body["metering_state"].as_str(),
            Some("outbox_pending")
        );
        let events: Vec<(String,Value)> = sqlx::query_as("SELECT entry_id,event FROM native_terminal_outbox WHERE object_id=$1 ORDER BY entry_id")
            .bind(uuid_at(&run,"/record_id")).fetch_all(f.cp.db().pool()).await.expect("only actually observed Eval model usage");
        assert_eq!(
            events.len(),
            1,
            "model B was accepted but never called, so there is no zero event for it"
        );
        assert_eq!(events[0].0, "model-a");
        assert_eq!(events[0].1["requested_model_id"], json!(first_model));
        assert_eq!(events[0].1["prompt_tokens"].as_u64(), Some(40));
        assert_eq!(events[0].1["completion_tokens"].as_u64(), Some(10));
        assert_eq!(events[0].1["total_tokens"].as_u64(), Some(50));
        let reconciled = wait_for_metering_state(
            &f.cp,
            uuid_at(&submission, "/operation_id"),
            "awaiting_reconciliation",
        )
        .await;
        assert_eq!(
            reconciled,
            (
                "awaiting_reconciliation".into(),
                Some("unpriced_meter".into()),
                2
            )
        );
        let stored: Value = sqlx::query_scalar("SELECT observed_usage FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
            .bind(f.cp.env().env_id).bind(uuid_at(&submission,"/operation_id")).fetch_one(f.cp.db().pool()).await.expect("the exact observed subset without fabricated entries");
        assert_eq!(stored, canonical_observed);
        let replay =
            f.dp.send(
                "POST",
                "/dp/v1/native-objects/complete-typed",
                Some(payload(actual.clone())),
            )
            .await;
        awaiting_unpriced(&replay, uuid_at(&submission, "/operation_id"));
        assert_eq!(outbox_count(&f.cp, &run).await, 1);
    }
}

// HTTP 受理 outbox 不能等同收费完成；只有真实价格、用量接收和账本记账才能 settled。
#[tokio::test]
async fn typed_token_metering_settles_only_after_real_priced_outbox_delivery() {
    let Some(f) = fixture().await else { return };
    let model = uuid_at(&f.submission, "/origin/model_id");
    support::world::insert_baseline_backdated(
        f.cp.db(),
        model,
        json!({
            "default":{"base":{"input":"2","output":"4","cache_read":"1"}}
        }),
        "2000-01-01T00:00:00Z"
            .parse()
            .expect("the real fixture price effective time"),
    )
    .await;
    let (submission, record, target) =
        claimed_target(&f, "response", "resp_typed_real_price_v12").await;
    let mut usage = token_usage("");
    let token_fields = usage[0]["meter"]["usage"]
        .as_object_mut()
        .expect("the explicit token report fields");
    token_fields.remove("error_class");
    token_fields.remove("error_message");
    let complete = typed_complete(&f.dp, &target, usage.clone()).await;
    assert_eq!(complete.status, 200, "{complete:?}");
    assert_eq!(complete.body["state"].as_str(), Some("completed"));
    assert_eq!(
        complete.body["metering_state"].as_str(),
        Some("outbox_pending"),
        "the same-transaction HTTP reply cannot claim a later ledger posting"
    );
    let operation = uuid_at(&submission, "/operation_id");
    let initial: (String,Value) = sqlx::query_as("SELECT state,receipt FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2 AND version=1")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("the original acceptance receipt survives any delivery race");
    assert_eq!(initial.0, "outbox_pending");
    let settled = wait_for_metering_state(&f.cp, operation, "settled").await;
    assert_eq!(settled, ("settled".into(), None, 2));
    let accounting: (bool,String,Option<i64>,Option<i64>) = sqlx::query_as(
        "SELECT o.delivered_at IS NOT NULL,c.status,c.charge_micro_cny,-l.amount_micro_cny FROM native_terminal_outbox o JOIN pricing_charges_cny c ON c.event_id=o.event_id::text LEFT JOIN billing_ledger_cny l ON l.event_id=o.event_id::text WHERE o.object_id=$1")
        .bind(uuid_at(&record,"/record_id")).fetch_one(f.cp.db().pool()).await.expect("the actual charge and ledger row support the settled decision");
    assert_eq!(
        accounting,
        (true, "charged".into(), Some(120), Some(120)),
        "40 input at 2 plus 10 output at 4 with group rate 1 must really post 120 micro-CNY"
    );
    let history: Vec<(i64,String,Value)> = sqlx::query_as("SELECT version,state,receipt FROM native_metering_receipts WHERE environment_id=$1 AND operation_id=$2 ORDER BY version")
        .bind(f.cp.env().env_id).bind(operation).fetch_all(f.cp.db().pool()).await.expect("the system delivery transition appends instead of rewriting acceptance");
    assert_eq!(history.len(), 2);
    assert_eq!((history[0].0, history[0].1.as_str()), (1, "outbox_pending"));
    assert_eq!((history[1].0, history[1].1.as_str()), (2, "settled"));
    assert_eq!(history[0].2, initial.1);
    assert_eq!(outbox_count(&f.cp, &record).await, 1);
    let replay = typed_complete(&f.dp, &target, usage).await;
    assert_eq!(replay.status, 200, "{replay:?}");
    assert_eq!(replay.body["state"].as_str(), Some("completed"));
    assert_eq!(replay.body["metering_state"].as_str(), Some("settled"));
    let version: i64 = sqlx::query_scalar("SELECT receipt_version FROM native_metering_records WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("replay cannot append another settlement receipt");
    assert_eq!(version, 2);
    assert_eq!(outbox_count(&f.cp, &record).await, 1);
}
