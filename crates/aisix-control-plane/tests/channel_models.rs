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

/// The direct document the channel row of `key` and `public` generated.
async fn direct_of(cp: &TestCp, key: Uuid, public: Uuid) -> Uuid {
    sqlx::query_scalar(
        "SELECT direct_model_id FROM channel_models \
         WHERE provider_key_id = $1 AND public_model_id = $2",
    )
    .bind(key)
    .bind(public)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the channel row")
}

/// The refusal's message at `path`.
fn message_at<'a>(r: &'a Resp, path: &str) -> Option<&'a str> {
    r.body["details"]
        .as_array()?
        .iter()
        .find(|d| d["path"] == path)?["message"]
        .as_str()
}

#[tokio::test]
async fn a_channels_direct_document_cannot_be_granted_to_a_customer() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5d9").await;
    let (public, _) = create_public(&cp, &op, "pub-grant-8c1").await;
    let r = put_channel(&cp, &op, key, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, key, public).await;
    assert!(
        is_live(&cp, Kind::Models, direct).await,
        "the direct document is live"
    );

    // Alone, and beside the public model: the refusal lands on the direct
    // document's own index, and nothing is granted.
    let tenant = tenant_granted(&cp, &op, &[]).await;
    let customer = "公共模型生成的直连模型不能授权给客户租户，请授权该公共模型";
    let r = set_grants(&cp, &op, &tenant.to_string(), &[direct]).await;
    assert_422_at(&r, "/allowed_model_ids/0");
    assert_eq!(
        message_at(&r, "/allowed_model_ids/0"),
        Some(customer),
        "{r:?}"
    );
    let r = set_grants(&cp, &op, &tenant.to_string(), &[public, direct]).await;
    assert_422_at(&r, "/allowed_model_ids/1");
    assert_eq!(grants(&cp, tenant).await, Vec::<Uuid>::new());

    // The public model itself is what a tenant is granted.
    let r = set_grants(&cp, &op, &tenant.to_string(), &[public]).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(grants(&cp, tenant).await, vec![public]);

    // Refused to the platform's own tenant too: the document has no price of
    // its own, and the platform's own tenant is priced like any other.
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(cp.db().pool())
        .await
        .expect("the internal tenant");
    let r = set_grants(&cp, &op, &internal.to_string(), &[direct]).await;
    assert_422_at(&r, "/allowed_model_ids/0");
    assert_eq!(
        message_at(&r, "/allowed_model_ids/0"),
        Some("公共模型生成的直连模型不能授权给平台内部租户，请授权该公共模型"),
        "{r:?}"
    );
    assert!(!grants(&cp, internal).await.contains(&direct));
    let r = set_grants(&cp, &op, &internal.to_string(), &[public]).await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// A baseline is the public model's: charges key on the public model's id,
/// so one written on the direct document a channel row generated would price
/// nothing. It is refused at `/model_id`, naming what to price instead.
#[tokio::test]
async fn a_channels_direct_document_takes_no_baseline() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5db").await;
    let (public, _) = create_public(&cp, &op, "pub-baseline-8c1").await;
    let r = put_channel(&cp, &op, key, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, key, public).await;
    assert!(
        is_live(&cp, Kind::Models, direct).await,
        "the direct document is live"
    );

    let now = support::world::db_now(&cp).await;
    let baseline = |model: Uuid| {
        json!({
            "model_id": model,
            "effective_from": now,
            "card": {"default": {"base": {"input": "4", "output": "20"}}},
            "note": "按官方价格页录入",
        })
    };
    let r = cp
        .send(
            "POST",
            "/admin/pricing/baselines",
            Some(op.cookie()),
            Some(baseline(direct)),
        )
        .await;
    assert_422_at(&r, "/model_id");
    assert_eq!(
        message_at(&r, "/model_id"),
        Some("该模型由公共模型生成，请为公共模型设置基准价"),
        "{r:?}"
    );
    let written: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pricing_baselines WHERE model_id = $1")
            .bind(direct)
            .fetch_one(cp.db().pool())
            .await
            .expect("count the direct document's baselines");
    assert_eq!(written, 0);

    // The public model is what takes the baseline.
    let r = cp
        .send(
            "POST",
            "/admin/pricing/baselines",
            Some(op.cookie()),
            Some(baseline(public)),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");

    // And the baseline list offers the public model, never the direct
    // document a baseline cannot be written on.
    let r = cp
        .send("GET", "/admin/pricing/baselines", Some(op.cookie()), None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let listed: Vec<&str> = r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|i| i["model_id"].as_str())
        .collect();
    assert!(listed.contains(&public.to_string().as_str()), "{r:?}");
    assert!(!listed.contains(&direct.to_string().as_str()), "{r:?}");
}

/// A group rate is the public model's too: a charge reads the rate of the
/// model the caller addressed, so an override written on the direct document
/// a channel row generated would never apply. It is refused at `/model_id`,
/// naming what to set instead, and the group's rate list does not offer the
/// direct document.
#[tokio::test]
async fn a_channels_direct_document_takes_no_group_rate() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5dc").await;
    let (public, _) = create_public(&cp, &op, "pub-rate-8c1").await;
    let r = put_channel(&cp, &op, key, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, key, public).await;
    assert!(
        is_live(&cp, Kind::Models, direct).await,
        "the direct document is live"
    );
    let shared: Uuid = sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = 'shared'")
        .fetch_one(cp.db().pool())
        .await
        .expect("the first-boot group");

    let now = support::world::db_now(&cp).await;
    let rate = |model: Uuid| json!({"model_id": model, "cny_per_usd": "6", "effective_from": now});
    let path = format!("/admin/provider-groups/{shared}/rates");
    let r = cp
        .send("POST", &path, Some(op.cookie()), Some(rate(direct)))
        .await;
    assert_422_at(&r, "/model_id");
    assert_eq!(
        message_at(&r, "/model_id"),
        Some("该模型由公共模型生成，请为公共模型设置换算率"),
        "{r:?}"
    );
    let written: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pricing_group_rates WHERE model_id = $1")
            .bind(direct)
            .fetch_one(cp.db().pool())
            .await
            .expect("count the direct document's rates");
    assert_eq!(written, 0);

    // The public model is what takes the override.
    let r = cp
        .send("POST", &path, Some(op.cookie()), Some(rate(public)))
        .await;
    assert_eq!(r.status, 201, "{r:?}");

    let r = cp.send("GET", &path, Some(op.cookie()), None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let listed: Vec<&str> = r.body["models"]
        .as_array()
        .expect("models")
        .iter()
        .filter_map(|m| m["model_id"].as_str())
        .collect();
    assert!(listed.contains(&public.to_string().as_str()), "{r:?}");
    assert!(!listed.contains(&direct.to_string().as_str()), "{r:?}");
}

/// A grant of a channel's direct document stored before grants of one were
/// refused, on the tenant or on a key's selection, is withdrawn by the next
/// boot, and the key documents are rewritten without it. The public model
/// stays granted. A later boot finds nothing to withdraw and rewrites
/// nothing.
#[tokio::test]
async fn a_stored_grant_of_a_channels_direct_document_is_withdrawn_at_boot() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5dd").await;
    let (public, _) = create_public(&cp, &op, "pub-sweep-8c1").await;
    let r = put_channel(&cp, &op, key, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, key, public).await;
    let shared: Uuid = sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = 'shared'")
        .fetch_one(cp.db().pool())
        .await
        .expect("the first-boot group");

    // A tenant granted the public model, with one key that follows every
    // grant and one that selects the public model.
    let tenant = tenant_granted(&cp, &op, &[public]).await;
    let mut keys = Vec::new();
    for body in [
        json!({"label": "follows-8c1", "group_id": shared}),
        json!({"label": "selects-8c1", "group_id": shared, "model_ids": [public]}),
    ] {
        let r = cp
            .send(
                "POST",
                &format!("/admin/tenants/{tenant}/keys"),
                Some(op.cookie()),
                Some(body),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
        keys.push(
            Uuid::parse_str(r.body["key"]["id"].as_str().expect("a key id")).expect("a uuid"),
        );
    }

    // What a release before the refusal could store: the direct document
    // granted beside the public model, and selected by the second key. The
    // API refuses both now, so they are written as that release left them.
    sqlx::query("UPDATE tenants SET allowed_model_ids = allowed_model_ids || $2 WHERE id = $1")
        .bind(tenant)
        .bind(vec![direct])
        .execute(cp.db().pool())
        .await
        .expect("seed the grant");
    sqlx::query("UPDATE tenant_keys SET model_ids = model_ids || $2 WHERE id = $1")
        .bind(keys[1])
        .bind(vec![direct])
        .execute(cp.db().pool())
        .await
        .expect("seed the selection");
    // A rename re-projects the tenant's keys from its rows, as that release
    // did after the grant: both key documents now name the direct document.
    let current = cp
        .send(
            "GET",
            &format!("/admin/tenants/{tenant}"),
            Some(op.cookie()),
            None,
        )
        .await;
    let version = current.body["version"].as_i64().expect("a version");
    let r = send_conditional(
        &cp,
        "PATCH",
        &format!("/admin/tenants/{tenant}"),
        op.cookie(),
        version,
        Some(json!({"name": format!("swept-{}", Uuid::new_v4().simple())})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let names = |doc: &Value, model: Uuid| -> bool {
        doc["allowed_model_ids"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| id == &json!(model.to_string())))
    };
    for k in &keys {
        let doc = live(&cp, Kind::ApiKeys, *k).await;
        assert!(names(&doc, direct), "before the boot, {k} names it: {doc}");
    }

    let booted = cp.restart().await;
    assert_eq!(grants(&booted, tenant).await, vec![public]);
    let selection: Option<Vec<Uuid>> =
        sqlx::query_scalar("SELECT model_ids FROM tenant_keys WHERE id = $1")
            .bind(keys[1])
            .fetch_one(booted.db().pool())
            .await
            .expect("read the key");
    assert_eq!(selection, Some(vec![public]));
    for k in &keys {
        let doc = live(&booted, Kind::ApiKeys, *k).await;
        assert!(!names(&doc, direct), "{k} no longer names it: {doc}");
        assert!(
            names(&doc, public),
            "{k} still names the public model: {doc}"
        );
    }

    // Idempotent: a second boot finds no such grant and touches no row.
    let versions = |pool: sqlx::PgPool| async move {
        let tenant_version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_one(&pool)
            .await
            .expect("read the tenant");
        let key_versions: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM tenant_keys WHERE tenant_id = $1 ORDER BY id")
                .bind(tenant)
                .fetch_all(&pool)
                .await
                .expect("read the keys");
        (tenant_version, key_versions)
    };
    let before = versions(booted.db().pool().clone()).await;
    let again = booted.restart().await;
    assert_eq!(versions(again.db().pool().clone()).await, before);
    assert_eq!(grants(&again, tenant).await, vec![public]);
}

/// The boot's withdrawal takes each holder's row before the resources lock,
/// as a model delete does: while a holder is held elsewhere, the boot waits
/// for it holding no resources lock, and withdraws the grant once it is
/// free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_boot_withdrawal_takes_the_tenant_before_the_resources_lock() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5de").await;
    let (public, _) = create_public(&cp, &op, "pub-sweep-order-8c1").await;
    let r = put_channel(&cp, &op, key, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, key, public).await;
    let tenant = tenant_granted(&cp, &op, &[public]).await;
    sqlx::query("UPDATE tenants SET allowed_model_ids = allowed_model_ids || $2 WHERE id = $1")
        .bind(tenant)
        .bind(vec![direct])
        .execute(cp.db().pool())
        .await
        .expect("seed the grant");

    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *holder)
        .await
        .expect("hold the tenant");
    let pool = cp.db().pool().clone();
    // Polled beside the boot, on this task: once the boot waits for the
    // tenant, whether the resources lock is free; then the tenant is let go.
    let watch = async move {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock')",
            )
            .fetch_one(&pool)
            .await
            .expect("read the waiters");
            if waiting {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the boot never waited for the held tenant"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let mut probe = pool.begin().await.expect("begin");
        let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(RESOURCES_LOCK_KEY)
            .fetch_one(&mut *probe)
            .await
            .expect("try the resources lock");
        probe.rollback().await.expect("end the probe");
        holder.rollback().await.expect("release the tenant");
        free
    };
    let (booted, free) = tokio::join!(cp.restart(), watch);
    assert!(
        free,
        "the boot held the resources lock while it waited for the tenant"
    );
    assert_eq!(grants(&booted, tenant).await, vec![public]);
}

#[tokio::test]
async fn a_key_with_no_selection_counts_toward_the_models_keys() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5da").await;
    let (public, _) = create_public(&cp, &op, "pub-keys-8c1").await;
    let r = put_channel(&cp, &op, key, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let shared: Uuid = sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = 'shared'")
        .fetch_one(cp.db().pool())
        .await
        .expect("the first-boot group");

    // One key with no selection in a tenant granted the model: it reaches
    // every grant, the model included. One in a tenant not granted it: it
    // reaches nothing of this model.
    let granted = tenant_granted(&cp, &op, &[public]).await;
    let other = tenant_granted(&cp, &op, &[]).await;
    for (tenant, label) in [(granted, "inherits-8c1"), (other, "elsewhere-8c1")] {
        let r = cp
            .send(
                "POST",
                &format!("/admin/tenants/{tenant}/keys"),
                Some(op.cookie()),
                Some(json!({"label": label, "group_id": shared})),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
        let selection: Option<Vec<Uuid>> =
            sqlx::query_scalar("SELECT model_ids FROM tenant_keys WHERE label = $1")
                .bind(label)
                .fetch_one(cp.db().pool())
                .await
                .expect("read the key");
        assert_eq!(selection, None, "the key selects no models of its own");
    }

    let view = public_model(&cp, &op, public).await;
    assert_eq!(
        (view["tenants"].clone(), view["keys"].clone()),
        (json!(1), json!(1)),
        "{view}"
    );

    // The retire refusal carries the same count.
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{public}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_in_use")),
        "{r:?}"
    );
    assert_eq!(r.body["counts"]["keys"], 1, "{r:?}");
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
async fn a_provider_key_a_retired_model_named_can_be_deleted() {
    // The sequence the review found unreachable: tick the model on the
    // account, disable that row, retire the public model, delete the
    // account. The row names a public model nobody can restore, so retiring
    // takes it with the documents, and the credential is not pinned forever.
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5dc").await;
    let (id, _) = create_public(&cp, &op, "pub-retired-8c1").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
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

    // The channel row went with the model.
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM channel_models WHERE public_model_id = $1",
    )
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("count the channel models");
    assert_eq!(rows, 0, "the retired model's channel rows go");

    // And the account can be deleted: the credential is not pinned.
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

#[tokio::test]
async fn a_channel_row_that_outlives_its_model_pins_nothing() {
    // The second line of defence: a `channel_models` row of a **retired**
    // public model pins no account, whatever route left it there. `retire`
    // deletes the rows it can see, so this writes one behind its back and
    // requires the delete to answer 200 anyway.
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5de").await;
    let (id, _) = create_public(&cp, &op, "pub-orphan-8c1").await;
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{id}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    sqlx::query(
        "INSERT INTO channel_models \
           (public_model_id, provider_key_id, enabled, upstream_model, priority, weight, direct_model_id) \
         VALUES ($1, $2, true, NULL, 0, 1, $3)",
    )
    .bind(id)
    .bind(key)
    .bind(Uuid::new_v4())
    .execute(cp.db().pool())
    .await
    .expect("plant a row its model never had");

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

/// `POST /admin/public-models/{id}/retire` in a task of its own, so the test
/// can decide when it is allowed to finish; its status and body.
fn spawn_retire(cp: &TestCp, cookie: &str, id: Uuid) -> tokio::task::JoinHandle<(u16, Value)> {
    let url = cp.url(&format!("{PUBLICS}/{id}/retire"));
    let cookie = cookie.to_string();
    tokio::spawn(async move {
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an HTTP client")
            .post(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .expect("send the retire");
        let status = response.status().as_u16();
        let text = response.text().await.expect("read the body");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    })
}

/// Whether a session of **this test's database** is queued for the resources
/// lock. `pg_locks` is cluster-wide and this file's tests run in parallel, so
/// a session elsewhere in the cluster — another test's, on the same key —
/// must not count: it says nothing about this transaction's place in the
/// queue.
async fn resources_lock_queued(cp: &TestCp) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
           AND classid::bigint = $1 AND objid::bigint = $2 AND objsubid = 1 \
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database()))",
    )
    .bind(RESOURCES_LOCK_KEY >> 32)
    .bind(RESOURCES_LOCK_KEY & 0xffff_ffff)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the queued locks")
}

/// Waits until a session is queued for the resources lock; fails after 10 s.
async fn until_resources_lock_queued(cp: &TestCp) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !resources_lock_queued(cp).await {
        assert!(
            std::time::Instant::now() < deadline,
            "no session queued for the resources lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retire_waits_for_the_resources_lock() {
    // The counts are read under the resources lock, and a grant commits under
    // that same lock, so a count taken before it can miss a grant that lands
    // in between. Here the grant is uncommitted while the retire waits, and
    // commits while it is still waiting: read after the lock, it is counted.
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (id, _) = create_public(&cp, &op, "pub-lock-8c1").await;
    let tenant = tenant_granted(&cp, &op, &[]).await;

    let mut held = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *held)
        .await
        .expect("hold the resources lock");
    // The grant `tenants::update` would write, uncommitted: under the
    // resources lock it holds itself, which this session stands in for.
    sqlx::query(
        "UPDATE tenants SET allowed_model_ids = ARRAY[$2]::uuid[], version = version + 1 \
         WHERE id = $1",
    )
    .bind(tenant)
    .bind(id)
    .execute(&mut *held)
    .await
    .expect("write the grant");

    let mut retire = spawn_retire(&cp, op.cookie(), id);
    // Queued on the lock means the retire has either counted already (the
    // bug) or is waiting to count (the fix); the commit lands after that
    // point either way, so the outcome tells the two apart.
    until_resources_lock_queued(&cp).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut retire)
            .await
            .is_err(),
        "the retire must wait for the resources lock"
    );
    held.commit().await.expect("release the resources lock");

    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(10), retire)
        .await
        .expect("the retire proceeds once the lock is free")
        .expect("join the retire");
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("public_model_in_use")),
        "the counts are read after the lock, so the committed grant is counted: {body}"
    );
    assert_eq!(body["counts"]["tenants"], 1, "the tenant it names: {body}");
    let retired: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT retired_at FROM public_models WHERE id = $1")
            .bind(id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the public model");
    assert!(retired.is_none(), "the model is not retired");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_that_commits_first_makes_the_retire_refuse() {
    // The grant takes the tenant row and then the resources lock; the retire
    // takes the public model's row and then the resources lock. Whichever
    // gets the lock first, the other sees it: a grant first is counted, so
    // the retire is refused naming that tenant rather than withdrawing a
    // model a live tenant now holds.
    //
    // The retire is held on the public model's row, which it takes before it
    // counts, so the grant's commit is guaranteed to precede the count: the
    // two locks are independent, and an advisory-lock wakeup order is not.
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-gp-5dd").await;
    let (id, _) = create_public(&cp, &op, "pub-race-8c1").await;
    let tenant = tenant_granted(&cp, &op, &[]).await;
    // A live routing document with no target: the grant has a model to name,
    // and the retire has no channel model holding it.
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = put_channel(&cp, &op, key, id, json!({"enabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");

    let mut held = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM public_models WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut *held)
        .await
        .expect("hold the public model's row");

    let retire = spawn_retire(&cp, op.cookie(), id);
    // The grant commits while the retire is still waiting for that row.
    let granted = set_grants(&cp, &op, &tenant.to_string(), &[id]).await;
    assert_eq!(granted.status, 200, "{granted:?}");
    held.commit().await.expect("release the public model's row");

    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(10), retire)
        .await
        .expect("the retire proceeds once the row is free")
        .expect("join the retire");
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("public_model_in_use")),
        "the retire counts the grant that committed first: {body}"
    );
    assert_eq!(body["counts"]["tenants"], 1, "the tenant it names: {body}");
    assert_eq!(grants(&cp, tenant).await, vec![id], "the grant stands");
    assert!(is_live(&cp, Kind::Models, id).await, "the model stays");
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
