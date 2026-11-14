//! 公开接口保持派生资源保护；内部派生写入在真实 PG 与 heartbeat 上验证构建门槛。

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::error::ApiError;
use aisix_control_plane::keys;
use aisix_control_plane::resources::write::{write_resource, ResourceWrite, WriteCtx, WriteOp};
use aisix_control_plane::resources::Kind;
use aisix_control_plane::tenants::TenantStatus;
use aisix_core::models::{ApiKey, EndpointFamily};
use futures::FutureExt;
use serde_json::{json, Value};
use support::{DpClient, OperatorSession, TestCp};
use uuid::Uuid;

const BUILD: &str = "dev+sha-12ab345";
const BASELINE: &str = "dev+sha-e411f9428fa1f744421c895f72a8c71940d5a4fc";
const NATIVE_FAMILIES: [&str; 5] = [
    "conversations",
    "containers",
    "vector_stores",
    "skills",
    "evals",
];

fn expected_endpoints() -> Value {
    json!([
        "chat_completions",
        "completions",
        "messages",
        "count_tokens",
        "responses",
        "embeddings",
        "models",
        "images",
        "audio",
        "videos",
        "rerank",
        "decisions",
        "realtime",
        "files",
        "batches",
        "fine_tuning",
        "moderations",
        "content_provenance_checks",
        "conversations",
        "containers",
        "vector_stores",
        "skills",
        "evals"
    ])
}

fn legacy_endpoints() -> Value {
    json!([
        "chat_completions",
        "completions",
        "messages",
        "count_tokens",
        "responses",
        "embeddings",
        "models"
    ])
}

async fn beat(client: &DpClient, dp_id: &str, version: &str) {
    let response = client
        .heartbeat(json!({
            "dp_id":dp_id, "instance_id":"native-key-gateway", "hostname":"native-key-host",
            "uptime_seconds":1, "version":version, "supported_guardrail_kinds":[],
            "applied_revision":1, "exporter_health":[], "partially_compatible_resources":[]
        }))
        .await;
    assert_eq!(response.status, 200, "{response:?}");
}

async fn projected(cp: &TestCp, key: &str) -> (Value, i32) {
    let doc: Value = sqlx::query_scalar(
        "SELECT doc FROM gw_resources WHERE kind='api_keys' AND id=$1::uuid AND NOT deleted",
    )
    .bind(key)
    .fetch_one(cp.db().pool())
    .await
    .expect("a persisted key document");
    let version: i32 = sqlx::query_scalar("SELECT doc_version FROM tenant_keys WHERE id=$1::uuid")
        .bind(key)
        .fetch_one(cp.db().pool())
        .await
        .expect("the key shape version");
    (doc, version)
}

// 与 tests/compat.rs 的派生写入相同：真实事务依次经过 schema、引用和 compat::gate。
// 这是内部组件回归；公开 HTTP 不能直接维护派生 api_keys。
async fn write_native_grant(cp: &TestCp, doc: Value) -> Result<i64, ApiError> {
    let ctx = WriteCtx::of(cp.state());
    cp.db()
        .tx(move |conn| {
            async move {
                Ok(write_resource(
                    conn,
                    &ctx,
                    ResourceWrite {
                        kind: Kind::ApiKeys,
                        id: Uuid::new_v4(),
                        op: WriteOp::Create(&doc),
                        expected_version: None,
                        managed_by: Some("tenant:native-key-floors-v14"),
                    },
                )
                .await?
                .version)
            }
            .boxed()
        })
        .await
}

async fn native_grant_rows(cp: &TestCp, doc: &Value) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE doc->>'key_hash'=$1")
        .bind(doc["key_hash"].as_str().expect("the grant's distinct hash"))
        .fetch_one(cp.db().pool())
        .await
        .expect("count the attempted derived grant")
}

async fn assert_public_grant_is_managed(cp: &TestCp, admin: &OperatorSession, doc: &Value) {
    let response = cp
        .send(
            "POST",
            "/admin/resources/api_keys",
            Some(admin.cookie()),
            Some(json!({"doc":doc})),
        )
        .await;
    assert_eq!(
        response.status, 409,
        "the public resource API cannot edit derived grants: {response:?}"
    );
    assert_eq!(response.body.get("code"), Some(&json!("managed_resource")));
    assert_eq!(native_grant_rows(cp, doc).await, 0);
}

async fn assert_native_grant_is_too_old(cp: &TestCp, doc: Value) {
    let error = write_native_grant(cp, doc.clone())
        .await
        .expect_err("the actual derived write must reach the version gate");
    assert_eq!(
        (error.status.as_u16(), error.code),
        (422, "gateway_too_old"),
        "{error:?}"
    );
    assert_eq!(error.details.len(), 1, "{error:?}");
    assert_eq!(error.details[0].path, "/allowed_endpoints");
    assert!(!error.details[0].message.is_empty());
    assert_eq!(
        native_grant_rows(cp, &doc).await,
        0,
        "a rejected grant is never persisted"
    );
}

#[test]
fn native_families_are_independent_grants_and_unknown_names_grant_nothing() {
    for name in NATIVE_FAMILIES {
        let family: EndpointFamily = serde_json::from_value(json!(name)).expect("decode family");
        assert_ne!(
            family,
            EndpointFamily::Unknown,
            "{name} is a recognized grant"
        );
        let key: ApiKey = serde_json::from_value(json!({
            "key_hash":"native-key-family", "allowed_endpoints":[name,"unreleased_family"]
        }))
        .expect("decode a key");
        assert!(key.can_call(family), "{name}");
        assert!(!key.can_call(EndpointFamily::Unknown));
        assert!(
            !key.can_call(EndpointFamily::Responses),
            "management does not grant inference"
        );
        for other in NATIVE_FAMILIES.into_iter().filter(|other| *other != name) {
            let other: EndpointFamily =
                serde_json::from_value(json!(other)).expect("decode family");
            assert!(!key.can_call(other), "{name} does not grant {other:?}");
        }
    }
}

#[tokio::test]
async fn native_families_require_the_new_build_without_borrowing_the_legacy_build_exception() {
    for cp_version in [BUILD, BASELINE] {
        let Some(cp) = TestCp::start_stamped(cp_version).await else {
            return;
        };
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let bundle = cp.issue_bundle(&admin, "native-key-floors").await;
        let client = cp.dp_client(&bundle);
        for gateway in [
            BASELINE,
            "dev+sha-e411f94",
            "dev+sha-08ff931",
            "dev",
            "dev+sha-98cd765",
        ] {
            beat(&client, &bundle.dp_id, gateway).await;
            for family in NATIVE_FAMILIES {
                let doc = json!({"key_hash":format!("native-refused-{}", Uuid::new_v4()),
                    "allowed_endpoints":[family]});
                assert_public_grant_is_managed(&cp, &admin, &doc).await;
                assert_native_grant_is_too_old(&cp, doc).await;
            }
        }
        if cp_version == BUILD {
            beat(&client, &bundle.dp_id, BUILD).await;
            for family in NATIVE_FAMILIES {
                let doc = json!({"key_hash":format!("native-allowed-{}",Uuid::new_v4()),
                    "allowed_endpoints":[family]});
                assert_public_grant_is_managed(&cp, &admin, &doc).await;
                let version = write_native_grant(&cp, doc.clone())
                    .await
                    .expect("the proven build accepts the internal derived grant");
                assert_eq!(version, 1, "{family}");
                assert_eq!(native_grant_rows(&cp, &doc).await, 1);
                let stored: Value = sqlx::query_scalar(
                    "SELECT doc FROM gw_resources WHERE doc->>'key_hash'=$1 AND NOT deleted",
                )
                .bind(doc["key_hash"].as_str().expect("the allowed grant hash"))
                .fetch_one(cp.db().pool())
                .await
                .expect("the exact allowed grant");
                assert_eq!(stored["allowed_endpoints"], json!([family]));
            }
        }
    }
}

#[tokio::test]
async fn e411_customer_mints_keep_seven_grants_and_catch_up_to_v6_after_gateway_upgrade() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let bundle = cp.issue_bundle(&admin, "native-key-fallback").await;
    let client = cp.dp_client(&bundle);
    beat(&client, &bundle.dp_id, BASELINE).await;
    let mut keys_created = Vec::new();
    for as_admin in [false, true] {
        let (path, cookie) = if as_admin {
            (format!("/admin/tenants/{tenant}/keys"), admin.cookie())
        } else {
            ("/api/keys".to_owned(), owner.cookie())
        };
        let mut body = json!({"label":format!("native-mint-{as_admin}"),"model_ids":[]});
        if as_admin {
            body["member_id"] = json!(owner.id);
        }
        let response = cp.send("POST", &path, Some(cookie), Some(body)).await;
        assert_eq!(response.status, 201, "{response:?}");
        let id = response
            .body
            .pointer("/key/id")
            .and_then(Value::as_str)
            .expect("new key id")
            .to_owned();
        let plaintext = response
            .body
            .get("plaintext")
            .and_then(Value::as_str)
            .expect("new key plaintext");
        let (doc, version) = projected(&cp, &id).await;
        assert_eq!(doc.get("allowed_endpoints"), Some(&legacy_endpoints()));
        assert_eq!(
            doc.get("key_hash"),
            Some(&json!(ApiKey::hash_bearer(plaintext)))
        );
        assert_eq!(version, 4);
        keys_created.push((id, doc));
    }
    let ctx = WriteCtx::of(cp.state());
    let pending = keys::catch_up(cp.db(), &ctx, 6)
        .await
        .expect("defer v6 expansion");
    assert_eq!((pending.rewritten, pending.deferred), (0, 1));
    assert!(pending.failed.is_empty());
    beat(&client, &bundle.dp_id, BUILD).await;
    let upgraded = keys::catch_up(cp.db(), &ctx, 6)
        .await
        .expect("allow v6 after gateway upgrade");
    assert_eq!((upgraded.rewritten, upgraded.deferred), (1, 0));
    assert!(upgraded.failed.is_empty());
    for (id, mut before) in keys_created {
        before["allowed_endpoints"] = expected_endpoints();
        let (doc, version) = projected(&cp, &id).await;
        assert_eq!(doc, before, "only the endpoint grants change");
        assert_eq!(version, 6);
    }
    let repeated = keys::catch_up(cp.db(), &ctx, 6)
        .await
        .expect("repeat a completed upgrade");
    assert_eq!((repeated.rewritten, repeated.deferred), (0, 0));
    assert!(repeated.failed.is_empty());
}

#[tokio::test]
async fn native_only_grants_refuse_an_incomplete_fleet_even_with_a_proven_build() {
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = cp.issue_bundle(&admin, "native-key-incomplete").await;
    let client = cp.dp_client(&bundle);
    beat(&client, &bundle.dp_id, BUILD).await;
    let updated =
        sqlx::query("UPDATE dp_credentials SET instance_cap_hit_at=now() WHERE dp_id=$1::uuid")
            .bind(&bundle.dp_id)
            .execute(cp.db().pool())
            .await
            .expect("record an incomplete fleet");
    assert_eq!(updated.rows_affected(), 1);
    for family in NATIVE_FAMILIES {
        let doc = json!({"key_hash":format!("native-capped-{}",Uuid::new_v4()),
            "allowed_endpoints":[family]});
        assert_public_grant_is_managed(&cp, &admin, &doc).await;
        assert_native_grant_is_too_old(&cp, doc).await;
    }
}
