//! 默认 Responses 固定计数的混合版本门禁；全部操作经过真实控制面与数据库。
mod support;

use aisix_control_plane::auth::OperatorRole;
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp};

const BUILD: &str = "dev+sha-a11ce4800000000000000000000000000000000";
const OLD: &str = "dev+sha-d887d328e6a033caf26ac7d6bbcae86efef1d5b2";

async fn create(cp: &TestCp, admin: &OperatorSession, kind: &str, doc: Value) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/resources/{kind}"),
        Some(admin.cookie()),
        Some(json!({"doc": doc})),
    )
    .await
}

async fn replace(
    cp: &TestCp,
    admin: &OperatorSession,
    kind: &str,
    id: &str,
    version: i64,
    doc: Value,
) -> Resp {
    cp.send_conditional(
        "PUT",
        &format!("/admin/resources/{kind}/{id}"),
        admin.cookie(),
        version,
        json!({"doc": doc}),
    )
    .await
}

fn account() -> Value {
    json!({
        "display_name": format!("default-responses-counter-{}", uuid::Uuid::new_v4()),
        "provider": "custom480",
        "adapter": "openai",
        "api_base": "https://counter.example.test/v1",
        "api_key": "test-only-counter"
    })
}

async fn counter(cp: &TestCp, admin: &OperatorSession, mut doc: Value) -> String {
    doc["display_name"] = json!(format!("counter-{}", uuid::Uuid::new_v4()));
    let response = create(cp, admin, "provider_keys", doc).await;
    assert_eq!(response.status, 201, "{response:?}");
    response.body["id"].as_str().expect("counter id").into()
}

fn setting(id: &str) -> Value {
    json!({"responses_input_tokens": {"provider_key_id": id}})
}

async fn beat(cp: &TestCp, admin: &OperatorSession, versions: &[&str]) {
    let bundle = cp.issue_bundle(admin, "responses-default-480").await;
    let client = cp.dp_client(&bundle);
    for (index, version) in versions.iter().enumerate() {
        let response = client
            .heartbeat(json!({
                "dp_id": bundle.dp_id,
                "instance_id": format!("responses-default-{index}"),
                "hostname": "responses-default-test",
                "version": version,
                "uptime_seconds": 1,
                "applied_revision": 0,
                "supported_guardrail_kinds": [],
                "exporter_health": []
            }))
            .await;
        assert_eq!(response.status, 200, "{response:?}");
    }
}

fn refused(response: &Resp, path: &str) {
    assert_eq!(response.status, 422, "{response:?}");
    assert_eq!(response.body["code"], "gateway_too_old", "{response:?}");
    assert_eq!(response.body["details"][0]["path"], path, "{response:?}");
}

// 若复用旧固定计数白名单，历史构建和混合实例会错误接受新默认语义。
#[tokio::test]
async fn default_responses_counter_requires_its_own_gateway_capability() {
    for (cp_version, provider, versions, allowed) in [
        (BUILD, "vercel", vec![OLD], false),
        (BUILD, "openrouter", vec![OLD], false),
        (BUILD, "custom480", vec![OLD], false),
        (BUILD, "custom", vec![OLD], false),
        (BUILD, "custom480", vec!["dev+sha-d887d32"], false),
        (BUILD, "custom480", vec![BUILD, OLD], false),
        (BUILD, "custom480", vec!["unknown"], false),
        (BUILD, "custom480", vec![BUILD], true),
        (
            "1.9.0+sha-a11ce48",
            "custom480",
            vec!["1.8.0+sha-d887d32"],
            false,
        ),
        (
            "1.9.0+sha-a11ce48",
            "custom480",
            vec!["1.9.0+sha-c0ffee0"],
            true,
        ),
    ] {
        let Some(cp) = TestCp::start_stamped(cp_version).await else {
            return;
        };
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let mut account = account();
        account["provider"] = json!(provider);
        let id = counter(&cp, &admin, account).await;
        beat(&cp, &admin, &versions).await;
        let response = create(&cp, &admin, "token_counting", setting(&id)).await;
        if allowed {
            assert_eq!(
                response.status, 201,
                "{cp_version} {versions:?}: {response:?}"
            );
        } else {
            refused(&response, "/responses_input_tokens");
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM gw_resources WHERE kind='token_counting' AND NOT deleted",
            )
            .fetch_one(cp.db().pool())
            .await
            .expect("count settings");
            assert_eq!(count, 0, "a refused setting must not be stored");
        }
    }
}

// 新门槛不能扩大到显式 Responses、内置账号或 Messages，撤销也必须一直可用。
#[tokio::test]
async fn existing_counter_shapes_and_unchanged_or_removed_defaults_keep_working() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let default = counter(&cp, &admin, account()).await;
    let response = create(&cp, &admin, "token_counting", setting(&default)).await;
    assert_eq!(response.status, 201, "{response:?}");
    beat(&cp, &admin, &[OLD]).await;
    let env = cp.env().env_id.to_string();
    let response = replace(&cp, &admin, "token_counting", &env, 1, setting(&default)).await;
    assert_eq!(response.status, 200, "unchanged default: {response:?}");
    let other = counter(&cp, &admin, account()).await;
    refused(
        &replace(&cp, &admin, "token_counting", &env, 2, setting(&other)).await,
        "/responses_input_tokens",
    );
    let response = replace(&cp, &admin, "token_counting", &env, 2, json!({})).await;
    assert_eq!(response.status, 200, "remove default: {response:?}");

    let mut explicit = account();
    explicit["apis"] = json!({"responses": {}});
    let explicit = counter(&cp, &admin, explicit).await;
    let native = counter(
        &cp,
        &admin,
        json!({"display_name":"openai", "provider":"openai", "api_key":"test-only"}),
    )
    .await;
    let messages = counter(
        &cp,
        &admin,
        json!({"display_name":"anthropic", "provider":"anthropic", "api_key":"test-only"}),
    )
    .await;
    for (version, doc) in [
        (3, setting(&explicit)),
        (4, setting(&native)),
        (
            5,
            json!({"messages_count_tokens": {"provider_key_id": messages}}),
        ),
    ] {
        let response = replace(&cp, &admin, "token_counting", &env, version, doc).await;
        assert_eq!(response.status, 200, "existing capability: {response:?}");
    }
    let mut disabled = account();
    disabled["apis"] = json!({});
    let disabled = counter(&cp, &admin, disabled).await;
    let response = replace(&cp, &admin, "token_counting", &env, 6, setting(&disabled)).await;
    assert_eq!(response.status, 422, "explicit disable: {response:?}");
    assert_ne!(response.body["code"], "gateway_too_old", "{response:?}");
}

// 只检查计数设置会被账号编辑绕过；未引用账号的相同编辑不该被阻止。
#[tokio::test]
async fn referenced_account_cannot_cross_into_new_defaults_on_old_gateways() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let mut explicit = account();
    explicit["apis"] = json!({"responses": {}});
    let id = counter(&cp, &admin, explicit.clone()).await;
    beat(&cp, &admin, &[OLD]).await;
    let response = create(&cp, &admin, "token_counting", setting(&id)).await;
    assert_eq!(response.status, 201, "{response:?}");
    refused(
        &replace(&cp, &admin, "provider_keys", &id, 1, account()).await,
        "/apis",
    );
    explicit["api_key"] = json!("test-only-rotated-key");
    let response = replace(&cp, &admin, "provider_keys", &id, 1, explicit.clone()).await;
    assert_eq!(response.status, 200, "credential rotation: {response:?}");
    let unreferenced = counter(&cp, &admin, explicit).await;
    let response = replace(&cp, &admin, "provider_keys", &unreferenced, 1, account()).await;
    assert_eq!(response.status, 200, "unreferenced account: {response:?}");
    let response = replace(
        &cp,
        &admin,
        "token_counting",
        &cp.env().env_id.to_string(),
        1,
        json!({}),
    )
    .await;
    assert_eq!(response.status, 200, "disable counter: {response:?}");
    let response = replace(&cp, &admin, "provider_keys", &id, 2, account()).await;
    assert_eq!(response.status, 200, "unbound account: {response:?}");
}

// 旧进程可以晚到且会加载此形状；applied revision 不能把运行时拒绝显示成已支持。
#[tokio::test]
async fn late_old_gateway_does_not_make_default_counter_rollout_applied() {
    use std::time::{Duration, Instant};
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let Some(cp) = TestCp::start_with(|cfg| {
        cfg.cp_version = BUILD.into();
        cfg.kine_endpoint = endpoint.clone();
    })
    .await
    else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = counter(&cp, &admin, account()).await;
    let response = create(&cp, &admin, "token_counting", setting(&id)).await;
    assert_eq!(response.status, 201, "{response:?}");
    let env = cp.env().env_id.to_string();
    let key = format!("{}token_counting/{env}", cp.env().env_prefix());
    let mut kine = etcd_client::Client::connect([endpoint], None)
        .await
        .expect("connect real kine");
    let deadline = Instant::now() + Duration::from_secs(30);
    let revision = loop {
        let response = kine.get(key.clone(), None).await.expect("read projection");
        if !response.kvs().is_empty() {
            break response.header().expect("revision").revision();
        }
        assert!(Instant::now() < deadline, "setting must project");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let provider_key = format!("{}provider_keys/{id}", cp.env().env_prefix());
    let projected = kine
        .get(provider_key, None)
        .await
        .expect("read account projection");
    let provider: Value = serde_json::from_slice(projected.kvs()[0].value()).expect("account JSON");
    assert!(
        provider.get("apis").is_none(),
        "projection must preserve absent apis"
    );
    let bundle = cp.issue_bundle(&admin, "late-responses-default-480").await;
    let client = cp.dp_client(&bundle);
    for (version, expected) in [(OLD, "rejected"), (BUILD, "applied")] {
        let response = client
            .heartbeat(json!({
                "dp_id": bundle.dp_id, "instance_id": "late-responses-default",
                "hostname": "late-responses-default", "version": version,
                "uptime_seconds": 1, "applied_revision": revision,
                "supported_guardrail_kinds": [], "exporter_health": []
            }))
            .await;
        assert_eq!(response.status, 200, "{response:?}");
        loop {
            let response = cp
                .send(
                    "GET",
                    &format!("/admin/resources/token_counting/{env}"),
                    Some(admin.cookie()),
                    None,
                )
                .await;
            assert_eq!(response.status, 200, "{response:?}");
            if response.body["rollout"]["state"] != "pending" {
                assert_eq!(response.body["rollout"]["state"], expected, "{response:?}");
                if expected == "rejected" {
                    assert!(!response.body["rollout"]["rejections"]
                        .as_array()
                        .expect("rejections")
                        .is_empty());
                }
                break;
            }
            assert!(
                Instant::now() < deadline,
                "projection acknowledgement: {response:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
