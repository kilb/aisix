//! 通过真实 CP HTTP 与 Postgres 验证 SIP 配置，所有账号值都是本地合成数据。

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::resources::{secrets, Kind};
use serde_json::{json, Value};
use support::world::{seed_key, seed_tenant};
use support::{OperatorSession, Resp, TestCp, TEST_MASTER_KEY};
use uuid::Uuid;

const SIGNING_SECRET: &str = "whsec_c2lwLWNvbmZpZy1zdGFuZGFyZC13ZWJob29rLWtleQ==";
const PROJECT: &str = "proj_sip_config_local_only";

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    provider: Uuid,
    model: Uuid,
    other_model: Uuid,
    key: String,
    member: Uuid,
}

fn id(value: &Value) -> Uuid {
    Uuid::parse_str(value["id"].as_str().expect("an explicit resource id"))
        .expect("canonical resource UUID")
}

async fn create(cp: &TestCp, admin: &OperatorSession, kind: &str, doc: Value) -> Uuid {
    let response = cp
        .send(
            "POST",
            &format!("/admin/resources/{kind}"),
            Some(admin.cookie()),
            Some(json!({"doc":doc})),
        )
        .await;
    assert_eq!(response.status, 201, "{response:?}");
    id(&response.body)
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let provider = create(&cp, &admin, "provider_keys", json!({
        "display_name":"sip-config-account", "provider":"openai", "api_key":"sk-local-sip-upstream",
        "api_base":"https://sip-config.example/v1", "groups":["shared"],
        "request":{"default_headers":{"OpenAI-Project":PROJECT}}
    })).await;
    let model = create(
        &cp,
        &admin,
        "models",
        json!({
            "display_name":"sip-config-direct", "provider":"openai", "model_name":"gpt-realtime",
            "provider_key_id":provider
        }),
    )
    .await;
    let other_provider = create(&cp, &admin, "provider_keys", json!({
        "display_name":"sip-config-other-account", "provider":"openai", "api_key":"sk-local-other",
        "groups":["shared"]
    })).await;
    let other_model = create(&cp, &admin, "models", json!({
        "display_name":"sip-config-other-direct", "provider":"openai", "model_name":"gpt-realtime",
        "provider_key_id":other_provider
    })).await;
    let tenant = seed_tenant(&cp, &admin).await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id=$1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("tenant version");
    let grant = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            admin.cookie(),
            version,
            json!({"allowed_model_ids":[model,other_model]}),
        )
        .await;
    assert_eq!(grant.status, 200, "{grant:?}");
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let key = seed_key(&cp, &admin, tenant, Some(member)).await;
    Some(Fixture {
        cp,
        admin,
        provider,
        model,
        other_model,
        key,
        member,
    })
}

async fn read(f: &Fixture, provider: Uuid) -> Value {
    let response =
        f.cp.get_with_cookie(
            &format!("/admin/resources/provider_keys/{provider}"),
            f.admin.cookie(),
        )
        .await;
    assert_eq!(response.status, 200, "{response:?}");
    response.body
}

fn binding(f: &Fixture) -> Value {
    json!({"route_id":"sip-local-route", "webhook_secret":SIGNING_SECRET,
        "api_key_id":f.key, "model_id":f.model})
}

async fn save(f: &Fixture, provider: Uuid, view: &Value, doc: Value) -> Resp {
    f.cp.send_conditional(
        "PUT",
        &format!("/admin/resources/provider_keys/{provider}"),
        f.admin.cookie(),
        view["version"].as_i64().expect("resource version"),
        json!({"doc":doc}),
    )
    .await
}

async fn attach(f: &Fixture) -> Value {
    let view = read(f, f.provider).await;
    let mut doc = view["doc"].clone();
    doc["realtime_sip"] = binding(f);
    let response = save(f, f.provider, &view, doc).await;
    assert_eq!(response.status, 200, "{response:?}");
    response.body
}

#[tokio::test]
async fn sip_binding_round_trips_sealed_redacted_and_audits_only_field_names() {
    let Some(f) = fixture().await else {
        return;
    };
    let view = attach(&f).await;
    assert_eq!(
        view.pointer("/doc/realtime_sip/webhook_secret"),
        Some(&json!("[REDACTED]"))
    );
    assert!(!view.to_string().contains(SIGNING_SECRET));
    assert_eq!(
        view.pointer("/doc/realtime_sip/api_key_id"),
        Some(&json!(f.key))
    );
    let mut stored: Value =
        sqlx::query_scalar("SELECT doc FROM gw_resources WHERE kind='provider_keys' AND id=$1")
            .bind(f.provider)
            .fetch_one(f.cp.db().pool())
            .await
            .expect("stored document");
    assert!(stored
        .pointer("/realtime_sip/webhook_secret")
        .and_then(Value::as_str)
        .is_some_and(|value| value.starts_with("aisixsealed.v1.")));
    assert!(!stored.to_string().contains(SIGNING_SECRET));
    secrets::unseal(
        &MasterKey(TEST_MASTER_KEY),
        Kind::ProviderKeys,
        f.provider,
        &mut stored,
    )
    .expect("projection opens the webhook credential");
    assert_eq!(
        stored.pointer("/realtime_sip/webhook_secret"),
        Some(&json!(SIGNING_SECRET))
    );
    let kept = save(&f, f.provider, &view, view["doc"].clone()).await;
    assert_eq!(kept.status, 200, "{kept:?}");
    let audits: Vec<Value> = sqlx::query_scalar("SELECT detail FROM audit_log")
        .fetch_all(f.cp.db().pool())
        .await
        .expect("audit details");
    assert!(audits
        .iter()
        .any(|detail| detail.to_string().contains("realtime_sip")));
    assert!(audits
        .iter()
        .all(|detail| !detail.to_string().contains(SIGNING_SECRET)
            && !detail.to_string().contains(PROJECT)));
}

#[tokio::test]
async fn sip_rebinding_cannot_reuse_redacted_secret_or_bypass_version() {
    let Some(f) = fixture().await else {
        return;
    };
    let view = attach(&f).await;
    for field in ["route_id", "api_key_id", "model_id"] {
        let mut doc = view["doc"].clone();
        doc["realtime_sip"][field] = if field == "route_id" {
            json!("sip-new-route")
        } else {
            json!(Uuid::new_v4())
        };
        let refused = save(&f, f.provider, &view, doc).await;
        assert_eq!(refused.status, 422, "{field}: {refused:?}");
        assert_eq!(
            refused.body.pointer("/details/0/path"),
            Some(&json!("realtime_sip.webhook_secret"))
        );
    }
    let mut changed_project = view["doc"].clone();
    changed_project["request"]["default_headers"]["OpenAI-Project"] = json!("proj_other_local");
    let refused = save(&f, f.provider, &view, changed_project).await;
    assert_eq!(refused.status, 422, "{refused:?}");
    assert_eq!(
        refused.body.pointer("/details/0/path"),
        Some(&json!("realtime_sip.webhook_secret"))
    );
    let missing_version =
        f.cp.send(
            "PUT",
            &format!("/admin/resources/provider_keys/{}", f.provider),
            Some(f.admin.cookie()),
            Some(json!({"doc":view["doc"]})),
        )
        .await;
    assert_eq!(missing_version.status, 428, "{missing_version:?}");
    let stale =
        f.cp.send_conditional(
            "PUT",
            &format!("/admin/resources/provider_keys/{}", f.provider),
            f.admin.cookie(),
            1,
            json!({"doc":view["doc"]}),
        )
        .await;
    assert_eq!(stale.status, 412, "{stale:?}");
}

#[tokio::test]
async fn sip_binding_refuses_wrong_account_disabled_owner_and_missing_static_project() {
    let Some(f) = fixture().await else {
        return;
    };
    let view = read(&f, f.provider).await;
    let mut wrong = view["doc"].clone();
    wrong["realtime_sip"] = binding(&f);
    wrong["realtime_sip"]["model_id"] = json!(f.other_model);
    let refused = save(&f, f.provider, &view, wrong).await;
    assert_eq!(refused.status, 422, "{refused:?}");
    assert_eq!(
        refused.body.pointer("/details/0/path"),
        Some(&json!("/realtime_sip/model_id"))
    );
    let mut no_project = view["doc"].clone();
    no_project["realtime_sip"] = binding(&f);
    no_project["request"]["default_headers"] = json!({});
    let refused = save(&f, f.provider, &view, no_project).await;
    assert_eq!(refused.status, 422, "{refused:?}");
    let disabled =
        f.cp.send(
            "PATCH",
            &format!("/admin/members/{}", f.member),
            Some(f.admin.cookie()),
            Some(json!({"disabled":true})),
        )
        .await;
    assert_eq!(disabled.status, 200, "{disabled:?}");
    let mut doc = view["doc"].clone();
    doc["realtime_sip"] = binding(&f);
    let refused = save(&f, f.provider, &view, doc).await;
    assert_eq!(refused.status, 422, "{refused:?}");
    assert_eq!(
        refused.body.pointer("/details/0/path"),
        Some(&json!("/realtime_sip/api_key_id"))
    );
}

#[tokio::test]
async fn sip_project_and_signing_key_cannot_be_split_across_provider_resources() {
    let Some(f) = fixture().await else {
        return;
    };
    attach(&f).await;
    let source = read(&f, f.provider).await;
    // 先复制普通账号再绑定，证明新增限制只作用于 SIP 启用时。
    let mut copied = source["doc"].clone();
    copied
        .as_object_mut()
        .expect("account object")
        .remove("realtime_sip");
    copied["display_name"] = json!("sip-duplicate-project");
    copied["api_key"] = json!("sk-different-key-same-static-project");
    copied["request"]["default_headers"]["OpenAI-Project"] = json!(PROJECT);
    let second = create(&f.cp, &f.admin, "provider_keys", copied).await;
    let second_model = create(
        &f.cp,
        &f.admin,
        "models",
        json!({
            "display_name":"sip-duplicate-direct", "provider":"openai", "model_name":"gpt-realtime",
            "provider_key_id":second
        }),
    )
    .await;
    let tenant = seed_tenant(&f.cp, &f.admin).await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id=$1")
        .bind(tenant)
        .fetch_one(f.cp.db().pool())
        .await
        .expect("tenant version");
    let grant =
        f.cp.send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            f.admin.cookie(),
            version,
            json!({"allowed_model_ids":[second_model]}),
        )
        .await;
    assert_eq!(grant.status, 200, "{grant:?}");
    let key = seed_key(&f.cp, &f.admin, tenant, None).await;
    let view = read(&f, second).await;
    for (secret, expected_field) in [
        (
            "whsec_ZGlmZmVyZW50LXNpZy1jb25maWcta2V5",
            "/realtime_sip/api_key_id",
        ),
        (
            "sip-config-standard-webhook-key",
            "/realtime_sip/webhook_secret",
        ),
    ] {
        let mut doc = view["doc"].clone();
        doc["realtime_sip"] = json!({"route_id":"sip-second-route", "webhook_secret":secret,
            "api_key_id":key, "model_id":second_model});
        let refused = save(&f, second, &view, doc).await;
        assert_eq!(refused.status, 422, "{secret}: {refused:?}");
        assert_eq!(
            refused.body.pointer("/details/0/path"),
            Some(&json!(expected_field))
        );
    }
}

#[tokio::test]
async fn sip_binding_does_not_prevent_revoking_its_original_caller() {
    let Some(f) = fixture().await else {
        return;
    };
    let bound = attach(&f).await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id=$1::uuid")
        .bind(&f.key)
        .fetch_one(f.cp.db().pool())
        .await
        .expect("caller version");
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
    let deleted: bool = sqlx::query_scalar(
        "SELECT deleted FROM gw_resources WHERE kind='api_keys' AND id=$1::uuid",
    )
    .bind(&f.key)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("revoked projected key");
    assert!(deleted);
    // 入口能直接移除，不需要读取或重新输入已经撤权身份的签名密钥。
    let mut detached = bound["doc"].clone();
    detached
        .as_object_mut()
        .expect("account document")
        .remove("realtime_sip");
    let detached = save(&f, f.provider, &bound, detached).await;
    assert_eq!(detached.status, 200, "{detached:?}");
    assert!(detached.body["doc"].get("realtime_sip").is_none());
}
