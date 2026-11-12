//! 真实控制平面上的探测权限和目标边界；这些场景不向上游发出请求。

mod support;

use aisix_control_plane::auth::OperatorRole;
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

async fn probe(cp: &TestCp, who: Option<&OperatorSession>, id: Uuid) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/models/{id}/probe"),
        who.map(OperatorSession::cookie),
        Some(json!({})),
    )
    .await
}

#[tokio::test]
async fn only_an_admin_can_spend_a_models_credential() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let id = Uuid::new_v4();
    let anonymous = probe(&cp, None, id).await;
    assert_eq!(anonymous.status, 401, "{anonymous:?}");
    for role in [OperatorRole::Viewer, OperatorRole::Operator] {
        let caller = cp.admin_login_seeded(role).await;
        let refused = probe(&cp, Some(&caller), id).await;
        assert_eq!(refused.status, 403, "{role:?}: {refused:?}");
    }
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let missing = probe(&cp, Some(&admin), id).await;
    assert_eq!(missing.status, 404, "{missing:?}");
}

async fn model(cp: &TestCp, admin: &OperatorSession, doc: Value) -> Uuid {
    let created = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": doc})),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    serde_json::from_value(created.body["id"].clone()).expect("模型 UUID")
}

#[tokio::test]
async fn a_composite_is_not_a_concrete_direct_target() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = model(
        &cp,
        &admin,
        json!({
            "display_name": "probe-routing",
            "routing": {"strategy": "round_robin", "targets": []}
        }),
    )
    .await;
    let result = probe(&cp, Some(&admin), id).await;
    assert_eq!(result.status, 200, "{result:?}");
    assert_eq!(result.body["model_id"], id.to_string());
    assert_eq!(result.body["status"], "unsupported");
    assert!(result.body.get("ttft_ms").is_some());
    assert_eq!(result.body["ttft_ms"], Value::Null);
    assert_eq!(result.body["usage"], Value::Null);
    assert_eq!(result.body["fidelity"]["status"], "not_tested");
}

async fn direct_model(cp: &TestCp, admin: &OperatorSession, display_name: &str) -> Uuid {
    let account = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": format!("probe-account-{}", Uuid::new_v4()),
                "provider": "openai", "api_key": "sk-probe-secret-never-sent-42",
                "api_base": "http://operator:secret-password@127.0.0.1:1/v1", "groups": ["shared"]
            }})),
        )
        .await;
    assert_eq!(account.status, 201, "{account:?}");
    model(
        cp,
        admin,
        json!({
            "display_name": display_name, "provider": "openai", "model_name": "concrete-upstream",
            "provider_key_id": account.body["id"]
        }),
    )
    .await
}

#[tokio::test]
async fn a_wildcard_never_substitutes_a_guessed_model() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "probe-*").await;
    let result = probe(&cp, Some(&admin), id).await;
    assert_eq!(result.status, 200, "{result:?}");
    assert_eq!(result.body["status"], "unsupported");
    assert_eq!(result.body["http_status"], Value::Null);
    assert_eq!(result.body["ttft_ms"], Value::Null);
}

#[tokio::test]
async fn a_blocked_target_is_failed_without_exposing_its_credentials() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "probe-concrete").await;
    let result = probe(&cp, Some(&admin), id).await;
    assert_eq!(result.status, 200, "{result:?}");
    assert_eq!(result.body["status"], "failed");
    for field in [
        "http_status",
        "ttft_ms",
        "usage",
        "output_tokens_per_second",
    ] {
        assert!(result.body.get(field).is_some(), "{field} 必须显式存在");
        assert_eq!(result.body[field], Value::Null);
    }
    let serialized = result.body.to_string();
    assert!(!serialized.contains("sk-probe-secret-never-sent-42"));
    assert!(!serialized.contains("secret-password"));
    assert!(!serialized.contains("operator:secret"));
    assert!(result.body["error"]
        .as_str()
        .is_some_and(|message| !message.is_empty()));
}

#[tokio::test]
async fn capability_checks_are_opt_in_and_never_follow_a_failed_base_probe() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "probe-capability-gate").await;
    for body in [json!({}), json!({"capability_check": true})] {
        let result = cp
            .send(
                "POST",
                &format!("/admin/models/{id}/probe"),
                Some(admin.cookie()),
                Some(body),
            )
            .await;
        assert_eq!(result.status, 200, "{result:?}");
        assert_eq!(result.body["status"], "failed");
        let capability = &result.body["capability_check"];
        assert_eq!(capability["status"], "not_tested");
        for field in ["total_duration_ms", "usage", "error"] {
            assert!(capability.get(field).is_some(), "{field} 必须显式存在");
            assert_eq!(capability[field], Value::Null);
        }
        for field in ["json", "instruction", "arithmetic", "sorting"] {
            assert!(capability["checks"].get(field).is_some());
            assert_eq!(capability["checks"][field], Value::Null);
        }
    }
}

#[tokio::test]
async fn no_probe_is_queued_when_all_execution_slots_are_held() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "probe-busy").await;
    let slots = cp
        .state()
        .model_probes
        .acquire_many(3)
        .await
        .expect("占满探测额度");
    let refused = probe(&cp, Some(&admin), id).await;
    assert_eq!(refused.status, 503, "{refused:?}");
    assert_eq!(refused.body["code"], "busy");
    drop(slots);
    let admitted = probe(&cp, Some(&admin), id).await;
    assert_eq!(
        admitted.status, 200,
        "释放额度后应执行地址检查：{admitted:?}"
    );
    assert_eq!(admitted.body["status"], "failed");
}
