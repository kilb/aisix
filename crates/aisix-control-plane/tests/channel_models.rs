//! Public models, channel models, and the gateway documents they generate.
//!
//! A public model is a name and an id; its id is the routing document's id,
//! minted once. Ticking a model on an upstream account writes one direct
//! document per enabled row and one routing document per public model,
//! through `write_resource`, owned by `public:<id>`. Disabling the last
//! serving account leaves the public model listed with no target, so a
//! request answers `upstream_unavailable` and the grants stay.
//!
//! Every expected value comes from what the test itself writes; nothing here
//! is read off the product.

mod support;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::channels;
use aisix_control_plane::resources::write::{WriteCtx, RESOURCES_LOCK_KEY};
use aisix_control_plane::resources::Kind;
use futures::FutureExt;
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// The operators' public-model routes.
const PUBLICS: &str = "/admin/public-models";

/// The stored document of `kind`/`id`, live or not: `(doc, deleted,
/// managed_by)`.
async fn stored(cp: &TestCp, kind: Kind, id: Uuid) -> Option<(Value, bool, Option<String>)> {
    let row: Option<(Value, bool, Option<String>)> = sqlx::query_as(
        "SELECT doc, deleted, managed_by FROM gw_resources WHERE kind = $1 AND id = $2",
    )
    .bind(kind.segment())
    .bind(id)
    .fetch_optional(cp.db().pool())
    .await
    .expect("read the row");
    row
}

/// The live document of `kind`/`id`, which must exist.
async fn live(cp: &TestCp, kind: Kind, id: Uuid) -> Value {
    let (doc, deleted, _) = stored(cp, kind, id)
        .await
        .unwrap_or_else(|| panic!("no {kind:?} row at {id}"));
    assert!(!deleted, "the {kind:?} row at {id} is a tombstone");
    doc
}

/// Whether a live document of `kind`/`id` exists.
async fn is_live(cp: &TestCp, kind: Kind, id: Uuid) -> bool {
    stored(cp, kind, id)
        .await
        .is_some_and(|(_, deleted, _)| !deleted)
}

/// `422` at exactly `path`.
fn assert_422_at(r: &Resp, path: &str) {
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (422, Some("validation_failed")),
        "{r:?}"
    );
    let paths: Vec<&str> = r.body["details"]
        .as_array()
        .map(|d| d.iter().filter_map(|x| x["path"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(paths, vec![path], "{r:?}");
}

/// An upstream account, created as `who`; its id.
async fn provider_key(cp: &TestCp, who: &OperatorSession, name: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(who.cookie()),
            Some(json!({"doc": {
                "display_name": name,
                "provider": "openai",
                "api_key": "sk-channel-8c1",
                "groups": ["shared"],
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("a provider key id")).expect("a uuid")
}

/// `POST /admin/public-models`, which must answer 201; the model's id and the
/// body it answered.
async fn create_public(cp: &TestCp, who: &OperatorSession, name: &str) -> (Uuid, Value) {
    let r = cp
        .send(
            "POST",
            PUBLICS,
            Some(who.cookie()),
            Some(json!({"name": name})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let id = Uuid::parse_str(r.body["id"].as_str().expect("a public model id")).expect("a uuid");
    (id, r.body)
}

/// `PUT /admin/provider-keys/{key}/channel-models/{public}` as `who`, with
/// `body`.
async fn put_channel(
    cp: &TestCp,
    who: &OperatorSession,
    key: Uuid,
    public: Uuid,
    body: Value,
) -> Resp {
    cp.send(
        "PUT",
        &format!("/admin/provider-keys/{key}/channel-models/{public}"),
        Some(who.cookie()),
        Some(body),
    )
    .await
}

/// `GET /admin/public-models/{id}`, which must answer 200; the body.
async fn public_model(cp: &TestCp, who: &OperatorSession, id: Uuid) -> Value {
    let r = cp
        .send("GET", &format!("{PUBLICS}/{id}"), Some(who.cookie()), None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body
}

/// `method path` as `cookie`, `If-Match: "<version>"` and `body` — the shape
/// every write carrying a version has.
async fn send_conditional(
    cp: &TestCp,
    method: &str,
    path: &str,
    cookie: &str,
    version: i64,
    body: Option<Value>,
) -> Resp {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
    let mut request = client
        .request(method, cp.url(path))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, cookie)
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""));
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

/// The tenant's `allowed_model_ids` as stored.
async fn grants(cp: &TestCp, tenant: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT allowed_model_ids FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the grants")
}

/// A tenant created and granted `grants`, as `who`; its id.
async fn tenant_granted(cp: &TestCp, who: &OperatorSession, grants: &[Uuid]) -> Uuid {
    let created = cp
        .send(
            "POST",
            "/admin/tenants",
            Some(who.cookie()),
            Some(json!({"name": format!("chan-tenant-{}", Uuid::new_v4().simple())})),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    let id = created.body["id"]
        .as_str()
        .expect("a tenant id")
        .to_string();
    let granted = set_grants(cp, who, &id, grants).await;
    assert_eq!(granted.status, 200, "{granted:?}");
    Uuid::parse_str(&id).expect("a uuid")
}

/// `PATCH /admin/tenants/{id}` setting `allowed_model_ids`, with the current
/// `If-Match`, as `who`.
async fn set_grants(cp: &TestCp, who: &OperatorSession, tenant: &str, grants: &[Uuid]) -> Resp {
    let current = cp
        .send(
            "GET",
            &format!("/admin/tenants/{tenant}"),
            Some(who.cookie()),
            None,
        )
        .await;
    assert_eq!(current.status, 200, "{current:?}");
    let version = current.body["version"].as_i64().expect("a version");
    let ids: Vec<String> = grants.iter().map(Uuid::to_string).collect();
    send_conditional(
        cp,
        "PATCH",
        &format!("/admin/tenants/{tenant}"),
        who.cookie(),
        version,
        Some(json!({"allowed_model_ids": ids})),
    )
    .await
}

#[tokio::test]
async fn a_public_model_is_one_row_and_no_document() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (id, body) = create_public(&cp, &op, "pub-x-8c1").await;
    assert_eq!(body["name"], "pub-x-8c1");
    assert!(body["served_by"].as_array().is_some_and(Vec::is_empty));
    assert!(body["retired_at"].is_null());

    // No document until a channel model exists.
    assert!(
        !is_live(&cp, Kind::Models, id).await,
        "a public model on its own writes no model document"
    );

    // And an ungranted id cannot be granted: there is no live model yet.
    let tenant = cp
        .send(
            "POST",
            "/admin/tenants",
            Some(op.cookie()),
            Some(json!({"name": format!("chan-{}", Uuid::new_v4().simple())})),
        )
        .await;
    assert_eq!(tenant.status, 201, "{tenant:?}");
    let r = set_grants(
        &cp,
        &op,
        tenant.body["id"].as_str().expect("a tenant id"),
        &[id],
    )
    .await;
    assert_422_at(&r, "/allowed_model_ids/0");
}

#[tokio::test]
async fn ticking_a_model_on_an_account_generates_both_documents() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5d2").await;
    let (id, _) = create_public(&cp, &op, "pub-x-8c1").await;

    let r = put_channel(&cp, &op, key, id, json!({"priority": 1, "weight": 1})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["public_model_id"], id.to_string());
    assert_eq!(r.body["served"], true);

    // The direct document, at the id the row minted.
    let direct_id: Uuid = sqlx::query_scalar(
        "SELECT direct_model_id FROM channel_models WHERE provider_key_id = $1 AND public_model_id = $2",
    )
    .bind(key)
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the channel row");
    let direct = live(&cp, Kind::Models, direct_id).await;
    assert_eq!(direct["display_name"], "pub-x-8c1#acct-gp-5d2");
    assert_eq!(direct["model_name"], "pub-x-8c1");
    assert_eq!(direct["provider"], "openai");
    assert_eq!(direct["provider_key_id"], key.to_string());

    // The routing document, at the public model's id.
    let routing = live(&cp, Kind::Models, id).await;
    assert_eq!(routing["display_name"], "pub-x-8c1");
    assert_eq!(routing["routing"]["strategy"], "round_robin");
    assert_eq!(
        routing["routing"]["targets"],
        json!([{"model_id": direct_id.to_string(), "priority": 1, "weight": 1}])
    );

    // Both owned by the public model, so only this module changes them.
    let owner = format!("public:{id}");
    for model in [id, direct_id] {
        let (_, _, managed_by) = stored(&cp, Kind::Models, model).await.expect("a row");
        assert_eq!(managed_by.as_deref(), Some(owner.as_str()), "{model}");
    }

    // The detail page names the account and its groups.
    let view = public_model(&cp, &op, id).await;
    assert_eq!(view["served_by"][0]["provider_key_id"], key.to_string());
    assert_eq!(view["served_by"][0]["display_name"], "acct-gp-5d2");
    assert_eq!(view["served_by"][0]["groups"], json!(["shared"]));
    assert_eq!(view["served_by"][0]["priority"], 1);
    assert_eq!(view["served_by"][0]["weight"], 1);
}

#[tokio::test]
async fn a_mapping_sets_the_upstream_name_and_null_means_the_public_name() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5d3").await;
    let (id, _) = create_public(&cp, &op, "pub-map-8c1").await;

    let r = put_channel(&cp, &op, key, id, json!({"upstream_model": "gpt-4o-mini"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["upstream_model"], "gpt-4o-mini");
    let direct_id: Uuid = sqlx::query_scalar(
        "SELECT direct_model_id FROM channel_models WHERE provider_key_id = $1 AND public_model_id = $2",
    )
    .bind(key)
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the channel row");
    assert_eq!(
        live(&cp, Kind::Models, direct_id).await["model_name"],
        "gpt-4o-mini"
    );

    // `null` clears it: the public name is sent upstream.
    let r = put_channel(&cp, &op, key, id, json!({"upstream_model": null})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body["upstream_model"].is_null(), "{r:?}");
    assert_eq!(
        live(&cp, Kind::Models, direct_id).await["model_name"],
        "pub-map-8c1"
    );

    // An absent field keeps the row's value.
    let r = put_channel(&cp, &op, key, id, json!({"upstream_model": "up-keep-8c1"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = put_channel(&cp, &op, key, id, json!({"priority": 3})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["upstream_model"], "up-keep-8c1", "{r:?}");
    assert_eq!(r.body["priority"], 3);

    // The upstream name is a shape, not a membership: anything in bounds.
    let r = put_channel(
        &cp,
        &op,
        key,
        id,
        json!({"upstream_model": "x".repeat(256)}),
    )
    .await;
    assert_422_at(&r, "/upstream_model");
    let r = put_channel(&cp, &op, key, id, json!({"upstream_model": ""})).await;
    assert_422_at(&r, "/upstream_model");
}

#[tokio::test]
async fn targets_are_ordered_by_priority_then_weight_then_id() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (id, _) = create_public(&cp, &op, "pub-order-8c1").await;
    let mut direct = Vec::new();
    for (i, (priority, weight)) in [(1, 1), (0, 5), (0, 9)].into_iter().enumerate() {
        let key = provider_key(&cp, &op, &format!("acct-order-{i}-8c1")).await;
        let r = put_channel(
            &cp,
            &op,
            key,
            id,
            json!({"priority": priority, "weight": weight}),
        )
        .await;
        assert_eq!(r.status, 200, "{r:?}");
        let direct_id: Uuid = sqlx::query_scalar(
            "SELECT direct_model_id FROM channel_models \
             WHERE provider_key_id = $1 AND public_model_id = $2",
        )
        .bind(key)
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the channel row");
        direct.push(direct_id);
    }

    let first = live(&cp, Kind::Models, id).await;
    let targets = first["routing"]["targets"]
        .as_array()
        .expect("targets")
        .clone();
    // priority 1 first; then the higher weight inside priority 0.
    assert_eq!(
        targets
            .iter()
            .map(|t| t["priority"].as_i64().expect("a priority"))
            .collect::<Vec<_>>(),
        vec![1, 0, 0]
    );
    assert_eq!(
        targets
            .iter()
            .map(|t| t["weight"].as_i64().expect("a weight"))
            .collect::<Vec<_>>(),
        vec![1, 9, 5]
    );
    assert_eq!(targets[1]["model_id"], direct[2].to_string());
    assert_eq!(targets[2]["model_id"], direct[1].to_string());

    // Two regenerations of unchanged rows are byte-identical.
    let ctx = WriteCtx::of(cp.state());
    cp.db()
        .tx(move |conn| {
            async move {
                channels::regenerate(conn, &ctx, id).await?;
                channels::regenerate(conn, &ctx, id).await?;
                Ok::<_, aisix_control_plane::error::ApiError>(())
            }
            .boxed()
        })
        .await
        .expect("regenerate twice");
    assert_eq!(live(&cp, Kind::Models, id).await, first);
}

#[tokio::test]
async fn a_generated_document_is_its_owners_to_change() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5d4").await;
    let (id, _) = create_public(&cp, &op, "pub-own-8c1").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct_id: Uuid = sqlx::query_scalar(
        "SELECT direct_model_id FROM channel_models WHERE provider_key_id = $1 AND public_model_id = $2",
    )
    .bind(key)
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the channel row");

    for model in [id, direct_id] {
        let current = cp
            .send(
                "GET",
                &format!("/admin/resources/models/{model}"),
                Some(op.cookie()),
                None,
            )
            .await;
        assert_eq!(current.status, 200, "{current:?}");
        let version = current.body["version"].as_i64().expect("a version");
        let put = send_conditional(
            &cp,
            "PUT",
            &format!("/admin/resources/models/{model}"),
            op.cookie(),
            version,
            Some(
                json!({"doc": {"display_name": "hand-edit-8c1", "provider": "openai",
                                    "model_name": "m", "provider_key_id": key.to_string()}}),
            ),
        )
        .await;
        assert_eq!(
            (put.status, put.body["code"].as_str()),
            (409, Some("managed_resource")),
            "PUT on {model}: {put:?}"
        );
        let delete = send_conditional(
            &cp,
            "DELETE",
            &format!("/admin/resources/models/{model}"),
            op.cookie(),
            version,
            None,
        )
        .await;
        assert_eq!(
            (delete.status, delete.body["code"].as_str()),
            (409, Some("managed_resource")),
            "DELETE on {model}: {delete:?}"
        );
    }
    // The generated documents are untouched by the refusals.
    assert_eq!(
        live(&cp, Kind::Models, id).await["display_name"],
        "pub-own-8c1"
    );
}

#[tokio::test]
async fn a_name_a_live_model_holds_cannot_be_a_public_model() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5d5").await;
    let r = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(op.cookie()),
            Some(json!({"doc": {
                "display_name": "taken-8c1",
                "provider": "openai",
                "model_name": "gpt-4o",
                "provider_key_id": key.to_string(),
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");

    let r = cp
        .send(
            "POST",
            PUBLICS,
            Some(op.cookie()),
            Some(json!({"name": "taken-8c1"})),
        )
        .await;
    assert_422_at(&r, "/name");

    for bad in ["*", "#", "a#b"] {
        let r = cp
            .send(
                "POST",
                PUBLICS,
                Some(op.cookie()),
                Some(json!({"name": format!("pub-{bad}-8c1")})),
            )
            .await;
        assert_422_at(&r, "/name");
    }
    let r = cp
        .send(
            "POST",
            PUBLICS,
            Some(op.cookie()),
            Some(json!({"name": "x".repeat(129)})),
        )
        .await;
    assert_422_at(&r, "/name");

    // The boundary is inclusive.
    let (_, body) = create_public(&cp, &op, &"y".repeat(128)).await;
    assert_eq!(body["name"].as_str().expect("a name").len(), 128);

    // A retired public model's name stays taken: its row keeps the id.
    let (id, _) = create_public(&cp, &op, "pub-retire-name-8c1").await;
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{id}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp
        .send(
            "POST",
            PUBLICS,
            Some(op.cookie()),
            Some(json!({"name": "pub-retire-name-8c1"})),
        )
        .await;
    assert_422_at(&r, "/name");
}

#[tokio::test]
async fn disabling_the_last_account_keeps_the_model_listed_with_no_target() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5d6").await;
    let (id, _) = create_public(&cp, &op, "pub-last-8c1").await;
    let direct_id: Uuid = {
        let r = put_channel(&cp, &op, key, id, json!({})).await;
        assert_eq!(r.status, 200, "{r:?}");
        sqlx::query_scalar(
            "SELECT direct_model_id FROM channel_models \
             WHERE provider_key_id = $1 AND public_model_id = $2",
        )
        .bind(key)
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the channel row")
    };
    // Granted only once the routing document exists: a grant names a live
    // model.
    assert!(is_live(&cp, Kind::Models, id).await);
    let tenant = tenant_granted(&cp, &op, &[id]).await;
    assert_eq!(grants(&cp, tenant).await, vec![id]);

    // Disabling the last serving account: the routing document stays, with
    // no target, and the direct document goes.
    let r = put_channel(&cp, &op, key, id, json!({"enabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let routing = live(&cp, Kind::Models, id).await;
    assert_eq!(routing["display_name"], "pub-last-8c1");
    assert_eq!(
        routing["routing"]["targets"],
        json!([]),
        "a public model with no enabled account serves nothing"
    );
    assert!(
        !is_live(&cp, Kind::Models, direct_id).await,
        "the disabled account's direct document is retracted"
    );

    // The owner's decision: the public model stays listed and the grants
    // are NOT withdrawn.
    assert_eq!(grants(&cp, tenant).await, vec![id]);
    let view = public_model(&cp, &op, id).await;
    assert_eq!(view["served_by"][0]["enabled"], false);
    assert_eq!(view["tenants"], 1);

    // Enabling it again restores the target, at the same direct document id:
    // the id is minted with the row, never per regeneration.
    let r = put_channel(&cp, &op, key, id, json!({"enabled": true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let routing = live(&cp, Kind::Models, id).await;
    assert_eq!(
        routing["routing"]["targets"],
        json!([{"model_id": direct_id.to_string(), "priority": 0, "weight": 1}])
    );

    // Deleting the channel model is the same retraction.
    let r = cp
        .send(
            "DELETE",
            &format!("/admin/provider-keys/{key}/channel-models/{id}"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    assert_eq!(
        live(&cp, Kind::Models, id).await["routing"]["targets"],
        json!([])
    );
    assert!(!is_live(&cp, Kind::Models, direct_id).await);
    assert_eq!(grants(&cp, tenant).await, vec![id]);
}

#[tokio::test]
async fn one_account_losing_the_model_keeps_the_other_serving() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let a = provider_key(&cp, &op, "acct-two-a-8c1").await;
    let b = provider_key(&cp, &op, "acct-two-b-8c1").await;
    let (id, _) = create_public(&cp, &op, "pub-two-8c1").await;
    for key in [a, b] {
        let r = put_channel(&cp, &op, key, id, json!({})).await;
        assert_eq!(r.status, 200, "{r:?}");
    }
    let direct_b: Uuid = sqlx::query_scalar(
        "SELECT direct_model_id FROM channel_models \
         WHERE provider_key_id = $1 AND public_model_id = $2",
    )
    .bind(b)
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read b's channel row");

    let r = put_channel(&cp, &op, a, id, json!({"enabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let targets = live(&cp, Kind::Models, id).await["routing"]["targets"].clone();
    assert_eq!(
        targets,
        json!([{"model_id": direct_b.to_string(), "priority": 0, "weight": 1}]),
        "only the enabled account is a target"
    );
}

#[tokio::test]
async fn a_retired_public_model_cannot_be_regenerated_or_retired_again() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5d7").await;
    let (id, _) = create_public(&cp, &op, "pub-retired-8c1").await;
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{id}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body["retired_at"].is_string(), "{r:?}");

    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{id}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_retired")),
        "{r:?}"
    );

    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_retired")),
        "{r:?}"
    );

    let ctx = WriteCtx::of(cp.state());
    let refused = cp
        .db()
        .tx(move |conn| {
            async move {
                channels::regenerate(conn, &ctx, id).await?;
                Ok::<_, aisix_control_plane::error::ApiError>(())
            }
            .boxed()
        })
        .await
        .expect_err("a retired public model is not regenerated");
    assert_eq!(refused.code, "public_model_retired");
}

#[tokio::test]
async fn a_public_model_in_use_cannot_be_retired() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5d8").await;
    let (id, _) = create_public(&cp, &op, "pub-inuse-8c1").await;
    let tenant = tenant_granted(&cp, &op, &[]).await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");

    // A granted tenant holds it.
    let granted = set_grants(&cp, &op, &tenant.to_string(), &[id]).await;
    assert_eq!(granted.status, 200, "{granted:?}");
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{id}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_in_use")),
        "{r:?}"
    );
    assert_eq!(r.body["counts"]["tenants"], 1, "{r:?}");
    assert_eq!(r.body["counts"]["channels"], 1, "{r:?}");
    assert_eq!(r.body["counts"]["keys"], 0, "{r:?}");

    // With the grant gone, the enabled channel model still holds it.
    let cleared = set_grants(&cp, &op, &tenant.to_string(), &[]).await;
    assert_eq!(cleared.status, 200, "{cleared:?}");
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{id}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_in_use")),
        "{r:?}"
    );
    assert_eq!(r.body["counts"]["tenants"], 0, "{r:?}");
    assert_eq!(r.body["counts"]["channels"], 1, "{r:?}");

    // With the channel model disabled too, it retires, and the documents go.
    let r = put_channel(&cp, &op, key, id, json!({"enabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{id}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(
        !is_live(&cp, Kind::Models, id).await,
        "the routing document goes"
    );
}

#[tokio::test]
async fn a_provider_key_serving_a_public_model_cannot_be_deleted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5d9").await;
    let (id, _) = create_public(&cp, &op, "pub-key-8c1").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");

    let current = cp
        .send(
            "GET",
            &format!("/admin/resources/provider_keys/{key}"),
            Some(op.cookie()),
            None,
        )
        .await;
    let version = current.body["version"].as_i64().expect("a version");
    let r = send_conditional(
        &cp,
        "DELETE",
        &format!("/admin/resources/provider_keys/{key}"),
        op.cookie(),
        version,
        None,
    )
    .await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("in_use")),
        "{r:?}"
    );
    assert_eq!(r.body["counts"]["public_models"], 1, "{r:?}");

    // Disabled rows still hold it: the operator named the account.
    let r = put_channel(&cp, &op, key, id, json!({"enabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let current = cp
        .send(
            "GET",
            &format!("/admin/resources/provider_keys/{key}"),
            Some(op.cookie()),
            None,
        )
        .await;
    let version = current.body["version"].as_i64().expect("a version");
    let r = send_conditional(
        &cp,
        "DELETE",
        &format!("/admin/resources/provider_keys/{key}"),
        op.cookie(),
        version,
        None,
    )
    .await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("in_use")),
        "{r:?}"
    );

    // Removing the channel model frees it.
    let r = cp
        .send(
            "DELETE",
            &format!("/admin/provider-keys/{key}/channel-models/{id}"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    let current = cp
        .send(
            "GET",
            &format!("/admin/resources/provider_keys/{key}"),
            Some(op.cookie()),
            None,
        )
        .await;
    let version = current.body["version"].as_i64().expect("a version");
    let r = send_conditional(
        &cp,
        "DELETE",
        &format!("/admin/resources/provider_keys/{key}"),
        op.cookie(),
        version,
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn regenerating_waits_for_the_public_model_row() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5da").await;
    let (id, _) = create_public(&cp, &op, "pub-head-8c1").await;
    let cp = std::sync::Arc::new(cp);

    // A side transaction holds the public model's row.
    let mut held = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM public_models WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut *held)
        .await
        .expect("lock the public model");

    let mut request = Box::pin(put_channel(&cp, &op, key, id, json!({})));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut request)
            .await
            .is_err(),
        "the channel edit must wait for the public model's row"
    );
    held.rollback().await.expect("release");
    let r = tokio::time::timeout(std::time::Duration::from_secs(10), request)
        .await
        .expect("the channel edit proceeds once the row is free");
    assert_eq!(r.status, 200, "{r:?}");
}

#[tokio::test]
async fn a_channel_edit_that_withdraws_takes_no_tenant_lock() {
    // The owner's decision: disabling the last serving account withdraws no
    // grant, so a channel edit takes no tenant row at all and cannot wait
    // behind one. This is the counter-case to the model delete, which does
    // take the tenant first.
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5db").await;
    let (id, _) = create_public(&cp, &op, "pub-notenant-8c1").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let tenant = tenant_granted(&cp, &op, &[id]).await;
    let cp = std::sync::Arc::new(cp);

    // Held tenant row: the disable must not wait on it.
    let mut held = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *held)
        .await
        .expect("lock the tenant");
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        put_channel(&cp, &op, key, id, json!({"enabled": false})),
    )
    .await
    .expect("a channel edit takes no tenant lock");
    assert_eq!(r.status, 200, "{r:?}");
    held.rollback().await.expect("release");

    // The grant is still there.
    assert_eq!(grants(&cp, tenant).await, vec![id]);
    // And nothing holds the resources lock while waiting for a tenant: the
    // edit finished while the tenant was held.
    let _ = RESOURCES_LOCK_KEY;
}

#[test]
fn a_direct_display_name_is_unique_per_public_model_and_account() {
    // Both halves are refused `#` by their own validators, so the pair is
    // unique: two public models differ in the first half, two accounts in the
    // second.
    assert_eq!(
        channels::direct_display_name("pub-8c1", "acct-a"),
        "pub-8c1#acct-a"
    );
    assert_ne!(
        channels::direct_display_name("pub-8c1", "acct-a"),
        channels::direct_display_name("pub-8c1", "acct-b")
    );
    assert_ne!(
        channels::direct_display_name("pub-8c1", "acct-a"),
        channels::direct_display_name("pub-8c2", "acct-a")
    );
    assert_eq!(
        channels::owner(Uuid::nil()),
        "public:00000000-0000-0000-0000-000000000000"
    );
}

#[test]
fn the_routing_document_is_a_function_of_the_rows() {
    let public = channels::PublicModelRow {
        id: Uuid::nil(),
        name: "pub-doc-8c1".to_string(),
        enabled: true,
        created_by: None,
        created_at: chrono::Utc::now(),
        retired_at: None,
    };
    let row = |id: u128, priority: i32, weight: i32| channels::ChannelModelRow {
        id: Uuid::from_u128(id),
        provider_key_id: Uuid::from_u128(1000 + id),
        public_model_id: Uuid::nil(),
        upstream_model: None,
        priority,
        weight,
        enabled: true,
        direct_model_id: Uuid::from_u128(id),
    };

    // No targets is the served-nothing state, and stays a routing document.
    let empty = channels::routing_doc(&public, &[]);
    assert_eq!(empty["display_name"], "pub-doc-8c1");
    assert_eq!(empty["routing"]["strategy"], "round_robin");
    assert_eq!(empty["routing"]["targets"], json!([]));
    assert!(empty.get("provider").is_none());

    // `round_robin`, never `failover`: failover would make weight inert.
    let doc = channels::routing_doc(&public, &[row(1, 0, 1), row(2, 2, 1)]);
    assert_eq!(doc["routing"]["strategy"], "round_robin");

    // The order is a function of the rows, not of their input order.
    let rows = [row(3, 0, 1), row(1, 1, 2), row(2, 1, 5)];
    let a = channels::routing_doc(&public, &rows);
    let mut reversed = rows.clone();
    reversed.reverse();
    let b = channels::routing_doc(&public, &reversed);
    assert_eq!(a, b);
    assert_eq!(
        a["routing"]["targets"]
            .as_array()
            .expect("targets")
            .iter()
            .map(|t| t["model_id"].as_str().expect("an id").to_string())
            .collect::<Vec<_>>(),
        vec![
            Uuid::from_u128(2).to_string(),
            Uuid::from_u128(1).to_string(),
            Uuid::from_u128(3).to_string(),
        ]
    );
}

#[tokio::test]
async fn who_may_manage_public_models() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;

    let r = cp.send("GET", PUBLICS, Some(viewer.cookie()), None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp
        .send(
            "POST",
            PUBLICS,
            Some(viewer.cookie()),
            Some(json!({"name": "pub-viewer-8c1"})),
        )
        .await;
    assert_eq!(r.status, 403, "{r:?}");

    let r = cp
        .send(
            "POST",
            PUBLICS,
            Some(operator.cookie()),
            Some(json!({"name": "pub-operator-8c1"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
}

/// Every document `channels` builds loads on the schema a gateway reads it
/// with, and is accepted by the strict schema the write path applies.
///
/// A gateway loads leniently and skips a row it cannot deserialize; a
/// generated document it cannot load is a public model that silently serves
/// nothing. Both shapes here — a direct document with a mapping, and a
/// routing document with and without targets — are built by
/// [`channels::direct_doc`] and [`channels::routing_doc`], so this is the
/// test the AGENTS.md rule asks of a new builder.
#[test]
fn every_generated_document_loads_on_both_schemas() {
    use aisix_control_plane::resources::validate::{validate_lenient, validate_strict};

    let public = channels::PublicModelRow {
        id: Uuid::new_v4(),
        name: "pub-gen-8c1".to_string(),
        enabled: true,
        created_by: None,
        created_at: chrono::Utc::now(),
        retired_at: None,
    };
    let channel = |upstream: Option<&str>, priority: i32, weight: i32| channels::ChannelModelRow {
        id: Uuid::new_v4(),
        provider_key_id: Uuid::new_v4(),
        public_model_id: public.id,
        upstream_model: upstream.map(str::to_string),
        priority,
        weight,
        enabled: true,
        direct_model_id: Uuid::new_v4(),
    };
    let mapped = channel(Some("gpt-4o-mini"), 1, 3);
    let plain = channel(None, 0, 1);
    let docs = vec![
        channels::direct_doc(&mapped, &public, "acct-gen-8c1", "openai"),
        channels::direct_doc(&plain, &public, "acct-gen-8c2", "anthropic"),
        channels::routing_doc(&public, &[mapped.clone(), plain.clone()]),
        // The served-nothing state: a routing document with no target.
        channels::routing_doc(&public, &[]),
    ];
    for doc in &docs {
        validate_strict(Kind::Models, doc)
            .unwrap_or_else(|e| panic!("strict refused a generated document: {e:?} {doc}"));
        validate_lenient(Kind::Models, doc)
            .unwrap_or_else(|e| panic!("a gateway would skip this document: {e:?} {doc}"));
    }
    // The canonical fallback: an upstream name absent means the public name.
    assert_eq!(docs[1]["model_name"], "pub-gen-8c1");
}
