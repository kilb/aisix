//! 兼容账号缺省支持 Responses；显式接口声明仍优先，使用真实控制面与数据库。
mod support;

use aisix_control_plane::auth::OperatorRole;
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp};

async fn account(cp: &TestCp, admin: &OperatorSession, name: &str, apis: Option<Value>) -> String {
    let mut doc = json!({
        "display_name": name,
        "provider": "custom-counting",
        "adapter": "openai",
        "api_key": "test-only-not-a-real-credential"
    });
    if let Some(apis) = apis {
        doc["apis"] = apis;
    }
    let response = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({"doc": doc})),
        )
        .await;
    assert_eq!(response.status, 201, "{response:?}");
    response.body["id"].as_str().expect("account id").to_owned()
}

fn counter(account_id: &str) -> Value {
    json!({"responses_input_tokens": {"provider_key_id": account_id}})
}

async fn read(cp: &TestCp, admin: &OperatorSession, path: &str, account_id: &str) -> Resp {
    let response = cp.send("GET", path, Some(admin.cookie()), None).await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.body["doc"]["responses_input_tokens"]["provider_key_id"], account_id,
        "{response:?}"
    );
    response
}

#[tokio::test]
async fn default_openai_compatible_counter_is_saved_reloaded_and_explicit_api_limits_remain() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let implicit = account(&cp, &admin, "implicit-responses", None).await;
    let null_apis = account(&cp, &admin, "null-api-responses", Some(Value::Null)).await;
    let declared = account(
        &cp,
        &admin,
        "declared-responses",
        Some(json!({"responses": {}})),
    )
    .await;
    let empty = account(&cp, &admin, "no-native-apis", Some(json!({}))).await;
    let messages = account(&cp, &admin, "only-messages", Some(json!({"messages": {}}))).await;
    let null_responses = account(
        &cp,
        &admin,
        "responses-disabled",
        Some(json!({"responses": null})),
    )
    .await;

    let created = cp
        .send(
            "POST",
            "/admin/resources/token_counting",
            Some(admin.cookie()),
            Some(json!({"doc": counter(&implicit)})),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    let path = format!("/admin/resources/token_counting/{}", cp.env().env_id);
    let mut stored = read(&cp, &admin, &path, &implicit).await;

    for account_id in [&null_apis, &declared] {
        let response = cp
            .send_conditional(
                "PUT",
                &path,
                admin.cookie(),
                stored.body["version"].as_i64().expect("stored version"),
                json!({"doc": counter(account_id)}),
            )
            .await;
        assert_eq!(response.status, 200, "{response:?}");
        stored = read(&cp, &admin, &path, account_id).await;
    }

    for account_id in [&empty, &messages, &null_responses] {
        let response = cp
            .send_conditional(
                "PUT",
                &path,
                admin.cookie(),
                stored.body["version"].as_i64().expect("stored version"),
                json!({"doc": counter(account_id)}),
            )
            .await;
        assert_eq!(response.status, 422, "{response:?}");
        assert_eq!(
            response.body["details"][0]["path"], "/responses_input_tokens/provider_key_id",
            "{response:?}"
        );
        let preserved = read(&cp, &admin, &path, &declared).await;
        assert_eq!(preserved.body["version"], stored.body["version"]);
    }
}
