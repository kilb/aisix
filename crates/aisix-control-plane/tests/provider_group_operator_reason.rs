//! 分组拒绝仅向运营区分；租户公开错误和直连可用率沿用现有合同。
mod support;

use aisix_control_plane::auth::MemberRole;
use serde_json::{json, Value};
use support::world::{deliver, seed_model, usage, world};
use uuid::Uuid;

#[tokio::test]
async fn provider_group_refusals_are_local_and_keep_the_tenant_outage_contract() {
    let Some(w) = world().await else { return };
    let target = seed_model(&w.cp, &w.admin).await;
    let provider: String = sqlx::query_scalar(
        "SELECT doc->>'provider_key_id' FROM gw_resources WHERE kind = 'models' AND id = $1",
    )
    .bind(target)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("模型所属账号");
    sqlx::query(
        "UPDATE tenants SET allowed_model_ids = array_append(allowed_model_ids, $2) WHERE id = $1",
    )
    .bind(w.tenant)
    .bind(target)
    .execute(w.cp.db().pool())
    .await
    .expect("授予测试模型");
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let mut rows = Vec::new();
    for (id, class) in [
        ("group-refusal", "provider_groups_unavailable"),
        ("actual-upstream-down", "transport"),
    ] {
        // 缺少显式 dispatch 字段也必须识别本地分类，不能把历史记录算进上游失败率。
        rows.push(usage(
            &w,
            id,
            target,
            json!({
                "model_id":target,"provider_key_id":provider,"status_code":503,
                "error_class":class,"error_message":"operator-only diagnostic",
                "is_final_attempt":true
            }),
        ));
    }
    let delivered = deliver(&w.dp, Uuid::new_v4(), rows).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(delivered.body.get("rejected"), Some(&json!(0)));

    let direct =
        w.cp.send(
            "GET",
            "/admin/usage/direct-models",
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(direct.status, 200, "{direct:?}");
    let measured = direct.body["models"]
        .as_array()
        .expect("直连模型清单")
        .iter()
        .find(|row| row["model"]["id"] == json!(target))
        .expect("目标模型");
    assert_eq!(measured.get("attributed_attempts"), Some(&json!(2)));
    assert_eq!(measured.get("excluded_attempts"), Some(&json!(1)));
    assert_eq!(measured.get("attempts"), Some(&json!(1)));
    assert_eq!(measured.get("failed_attempts"), Some(&json!(1)));
    assert_eq!(measured.get("unmeasured_attempts"), Some(&json!(0)));

    let tenant =
        w.cp.send(
            "GET",
            "/api/usage/calls?outcome=failed&error_class=upstream_unavailable",
            Some(owner.cookie()),
            None,
        )
        .await;
    assert_eq!(tenant.status, 200, "{tenant:?}");
    let calls = tenant.body["items"].as_array().expect("租户调用清单");
    assert_eq!(calls.len(), 2, "{tenant:?}");
    for call in calls {
        assert_eq!(
            call.get("error_class"),
            Some(&json!("upstream_unavailable"))
        );
        assert_eq!(call.get("status_code"), Some(&Value::Null));
    }
    assert!(!tenant.body.to_string().contains("provider_groups"));
    assert!(!tenant.body.to_string().contains("operator-only"));
    let refused_filter =
        w.cp.send(
            "GET",
            "/api/usage/calls?error_class=provider_groups_unavailable",
            Some(owner.cookie()),
            None,
        )
        .await;
    assert_eq!(refused_filter.status, 422, "{refused_filter:?}");
}
