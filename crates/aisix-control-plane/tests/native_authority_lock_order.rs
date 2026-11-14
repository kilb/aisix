//! 用真实事务交错验证同一准入重放与已接受结果绑定不会互相锁死。

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::auth::OperatorRole;
use ring::hmac;
use serde_json::{json, Value};
use sqlx::PgPool;
use support::world::{seed_key, seed_tenant};
use support::{DpClient, TestCp};
use uuid::Uuid;

const BASE: &str = "https://native-lock-order.example/v1";
const CREDENTIAL: &str = "sk-native-lock-order-fixture";
const RELATED_BEGIN: &str = "/dp/v1/native-objects/related/begin";
const RELATED_BIND: &str = "/dp/v1/native-objects/related/bind";

fn uuid_at(value: &Value, field: &str) -> Uuid {
    Uuid::parse_str(value[field].as_str().expect("an explicit UUID field")).expect("a valid UUID")
}

// 单独编码 origin 线协议，不用被测产品计算预期指纹。
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
    dp: Arc<DpClient>,
    parent: Uuid,
    admission: Value,
    binding: Value,
}

async fn fixture() -> Fixture {
    let cp = TestCp::start()
        .await
        .expect("CP_TEST_DATABASE_URL must name the isolated test Postgres server");
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let provider = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name":"lock-order-account", "provider":"openai", "api_key":CREDENTIAL,
                "api_base":BASE, "groups":["shared"]
            }})),
        )
        .await;
    assert_eq!(provider.status, 201, "{provider:?}");
    let provider = uuid_at(&provider.body, "id");
    let model = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name":"lock-order-model", "provider":"openai", "model_name":"gpt-4o-mini",
                "provider_key_id":provider
            }})),
        )
        .await;
    assert_eq!(model.status, 201, "{model:?}");
    let model = uuid_at(&model.body, "id");
    let tenant = seed_tenant(&cp, &admin).await;
    let path = format!("/admin/tenants/{tenant}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200, "{current:?}");
    let granted = cp
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            current.body["version"]
                .as_i64()
                .expect("the tenant version"),
            json!({"allowed_model_ids":[model]}),
        )
        .await;
    assert_eq!(granted.status, 200, "{granted:?}");
    let key = Uuid::parse_str(&seed_key(&cp, &admin, tenant, None).await).expect("the tenant key");
    let dp = Arc::new(cp.issue_dp_client("native-lock-order").await);
    let mut submission = json!({
        "operation_id":Uuid::new_v4(), "api_key_id":key, "protocol":"openai", "kind":"vector_store",
        "origin":{"model_id":model,"provider_key_id":provider,"boundary":boundary(&cp, provider)},
        "requested_model_id":model, "requested_model":"lock-order-model", "upstream_model":"gpt-4o-mini",
        "operation":"vector_stores", "request_id":"native-lock-order", "asynchronous":false
    });
    let begun = dp
        .send(
            "POST",
            "/dp/v1/native-objects/begin",
            Some(submission.clone()),
        )
        .await;
    assert_eq!(begun.status, 200, "{begun:?}");
    let parent = dp.send("POST", "/dp/v1/native-objects/bind", Some(json!({
        "operation_id":submission["operation_id"], "api_key_id":key, "raw_id":"vs_lock_order"
    }))).await;
    assert_eq!(parent.status, 200, "{parent:?}");
    let parent = uuid_at(&parent.body["record"], "record_id");
    submission["operation_id"] = json!(Uuid::new_v4());
    submission["kind"] = json!("vector_store_file_batch");
    submission["parent_id"] = json!(parent);
    let admission = json!({
        "submission":submission, "owner_parent_record_id":parent,
        "references":[{"record_id":parent,"consumer_family":"vector_stores"}]
    });
    let begun = dp
        .send("POST", RELATED_BEGIN, Some(admission.clone()))
        .await;
    assert_eq!(begun.status, 200, "{begun:?}");
    assert_eq!(begun.body["state"].as_str(), Some("submitted"));
    let binding = json!({
        "binding":{"operation_id":submission["operation_id"], "api_key_id":key, "raw_id":"vsfb_lock_order"},
        "relations":[{"parent":{"source":"record","record_id":parent},
            "child":{"source":"root"},"relation":"vector_store_batch"}]
    });
    Fixture {
        cp,
        dp,
        parent,
        admission,
        binding,
    }
}

// 同时接受行锁和事务 advisory 锁，断言等待依赖而不把修复手段写入合同。
async fn until_waiting_on(pool: &PgPool, blocker: i32, waiter: Option<i32>) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let pid: Option<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity WHERE datid=(SELECT oid FROM pg_database WHERE datname=current_database()) \
             AND $1=ANY(pg_blocking_pids(pid)) AND ($2::integer IS NULL OR pid=$2) ORDER BY pid LIMIT 1",
        ).bind(blocker).bind(waiter).fetch_optional(pool).await.expect("read the real wait graph");
        if let Some(pid) = pid {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "no request queued behind backend {blocker}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn replay_racing_bind(plain: bool) {
    let f = fixture().await;
    let operation = uuid_at(&f.admission["submission"], "operation_id");
    let mut holder =
        f.cp.db()
            .pool()
            .begin()
            .await
            .expect("begin the independent blocker");
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .expect("the blocker's backend");
    let query = if plain {
        "SELECT operation_id FROM native_operations WHERE environment_id=$1 AND operation_id=$2 FOR UPDATE"
    } else {
        "SELECT operation_id FROM native_related_operations WHERE environment_id=$1 AND operation_id=$2 FOR UPDATE"
    };
    sqlx::query(query)
        .bind(f.cp.env().env_id)
        .bind(operation)
        .execute(&mut *holder)
        .await
        .expect("hold the accepted operation's checkpoint");

    let dp = f.dp.clone();
    let binding = f.binding.clone();
    let bind = tokio::spawn(async move {
        let (path, body) = if plain {
            ("/dp/v1/native-objects/bind", binding["binding"].clone())
        } else {
            (RELATED_BIND, binding)
        };
        dp.send("POST", path, Some(body)).await
    });
    let bind_pid = until_waiting_on(f.cp.db().pool(), holder_pid, None).await;
    let dp = f.dp.clone();
    let admission = f.admission.clone();
    let replay = tokio::spawn(async move { dp.send("POST", RELATED_BEGIN, Some(admission)).await });
    // 旧实现此时拿着 parent 等 submission；修复后在拿任何行锁前排队。
    until_waiting_on(f.cp.db().pool(), bind_pid, None).await;
    holder
        .commit()
        .await
        .expect("release the forced interleaving");
    let (bound, replayed) = tokio::join!(bind, replay);
    let bound = bound.expect("the accepted bind task");
    let replayed = replayed.expect("the admission replay task");
    assert_eq!(
        bound.status, 200,
        "accepted bind must survive a concurrent replay: {bound:?}; replay: {replayed:?}"
    );
    assert_eq!(
        replayed.status, 200,
        "idempotent admission replay must complete: {replayed:?}"
    );
    assert_eq!(replayed.body["operation_id"], json!(operation));

    // 普通 bind 先保存 root 后，related bind 仍须原子、幂等地补全同一成员集合。
    let receipt =
        f.dp.send("POST", RELATED_BIND, Some(f.binding.clone()))
            .await;
    assert_eq!(receipt.status, 200, "{receipt:?}");
    if !plain {
        assert_eq!(receipt.body, bound.body);
    }
    let repeated =
        f.dp.send("POST", RELATED_BIND, Some(f.binding.clone()))
            .await;
    assert_eq!(repeated.status, 200, "{repeated:?}");
    assert_eq!(repeated.body, receipt.body);
    let root = uuid_at(&receipt.body["record"], "record_id");
    assert_eq!(
        receipt.body["record"]["raw_id"].as_str(),
        Some("vsfb_lock_order")
    );
    assert_eq!(receipt.body["record"]["parent_id"], json!(f.parent));
    let memberships = receipt.body["memberships"]
        .as_array()
        .expect("explicit membership set");
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0]["parent_record_id"], json!(f.parent));
    assert_eq!(memberships[0]["child_record_id"], json!(root));
    assert_eq!(
        memberships[0]["relation"].as_str(),
        Some("vector_store_batch")
    );
    assert_eq!(memberships[0]["generation"].as_i64(), Some(1));
    let roots: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM native_objects WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(f.cp.env().env_id)
    .bind(operation)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("the persisted root count");
    assert_eq!(roots, 1);
    let memberships: Vec<(Uuid, Uuid, String, i64, bool)> = sqlx::query_as(
        "SELECT parent_id,child_id,relation,generation,deleted_at IS NULL FROM native_object_memberships WHERE environment_id=$1 AND created_operation_id=$2",
    ).bind(f.cp.env().env_id).bind(operation).fetch_all(f.cp.db().pool()).await.expect("the complete persisted membership set");
    assert_eq!(
        memberships,
        vec![(f.parent, root, "vector_store_batch".into(), 1, true)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn related_admission_replay_and_accepted_bind_complete_without_deadlock() {
    replay_racing_bind(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn related_admission_replay_and_plain_accepted_bind_complete_without_deadlock() {
    replay_racing_bind(true).await;
}
