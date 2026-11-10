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
use aisix_control_plane::housekeeping;
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
                channels::regenerate(conn, &ctx, id, &[]).await?;
                channels::regenerate(conn, &ctx, id, &[]).await?;
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
                channels::regenerate(conn, &ctx, id, &[]).await?;
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

/// Every `channel_models` row `public` has, across accounts.
async fn channel_rows(cp: &TestCp, public: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*)::bigint FROM channel_models WHERE public_model_id = $1")
        .bind(public)
        .fetch_one(cp.db().pool())
        .await
        .expect("count the channel rows")
}

/// `POST /admin/public-models/{id}/delete` as `who`.
async fn delete_public(cp: &TestCp, who: &OperatorSession, id: Uuid) -> Resp {
    cp.send(
        "POST",
        &format!("{PUBLICS}/{id}/delete"),
        Some(who.cookie()),
        Some(json!({})),
    )
    .await
}

/// The audit detail of the newest row for `action` on `public`.
async fn audit_detail(cp: &TestCp, action: &str, public: Uuid) -> Value {
    sqlx::query_scalar(
        "SELECT detail FROM audit_log \
         WHERE action = $1 AND target_kind = 'public_model' AND target_id = $2 \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(action)
    .bind(public.to_string())
    .fetch_one(cp.db().pool())
    .await
    .expect("read the audit row")
}

#[tokio::test]
async fn deleting_a_public_model_removes_every_accounts_mapping_and_both_documents() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let first = provider_key(&cp, &op, "acct-del-two-8c2").await;
    let second = provider_key(&cp, &op, "acct-del-two-8c3").await;
    let (id, _) = create_public(&cp, &op, "pub-del-two-8c2").await;
    for key in [first, second] {
        let r = put_channel(&cp, &op, key, id, json!({})).await;
        assert_eq!(r.status, 200, "{r:?}");
    }
    let direct_first = direct_of(&cp, first, id).await;
    let direct_second = direct_of(&cp, second, id).await;
    assert!(is_live(&cp, Kind::Models, direct_first).await);
    assert!(is_live(&cp, Kind::Models, direct_second).await);
    assert_eq!(channel_rows(&cp, id).await, 2);

    // Two enabled accounts serve it, which refuses a retire and must not
    // refuse this: the mapping is what the delete is for.
    let r = delete_public(&cp, &op, id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body["retired_at"].is_string(), "{r:?}");
    // Every mapping, every document it generated, and the routing document.
    assert_eq!(channel_rows(&cp, id).await, 0, "the mappings go");
    assert!(
        !is_live(&cp, Kind::Models, id).await,
        "the routing document goes"
    );
    assert!(!is_live(&cp, Kind::Models, direct_first).await);
    assert!(!is_live(&cp, Kind::Models, direct_second).await);
    // Both tombstones name the owner, so a hand edit cannot revive one.
    for gone in [id, direct_first, direct_second] {
        let (_, deleted, managed_by) = stored(&cp, Kind::Models, gone)
            .await
            .unwrap_or_else(|| panic!("no row at {gone}"));
        assert!(deleted, "the row at {gone} is a tombstone");
        assert_eq!(managed_by.as_deref(), Some(channels::owner(id).as_str()));
    }
    // The account's checklist no longer lists it, and the row is retired.
    let r = cp
        .send(
            "GET",
            &format!("/admin/provider-keys/{first}/channel-models"),
            Some(op.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(
        !r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|i| i["public_model_id"] == json!(id)),
        "{r:?}"
    );
    // The audit row says what the delete took, not only its name.
    assert_eq!(
        audit_detail(&cp, "public_model.delete", id).await,
        json!({"name": "pub-del-two-8c2", "tenants": 0, "keys": 0, "channels": 2})
    );
}

#[tokio::test]
async fn a_delete_a_tenant_grant_holds_is_refused_and_deletes_nothing() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-del-grant-8c2").await;
    let (id, _) = create_public(&cp, &op, "pub-del-grant-8c2").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, key, id).await;
    let tenant = tenant_granted(&cp, &op, &[id]).await;

    let r = delete_public(&cp, &op, id).await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_in_use")),
        "{r:?}"
    );
    assert_eq!(r.body["counts"]["tenants"], 1, "{r:?}");
    assert_eq!(r.body["counts"]["keys"], 0, "{r:?}");
    // The serving account is reported but is not why it was refused.
    assert_eq!(r.body["counts"]["channels"], 1, "{r:?}");

    // One transaction: the refusal left the mapping and both documents alone.
    assert_eq!(channel_rows(&cp, id).await, 1, "the mapping is still there");
    assert!(is_live(&cp, Kind::Models, id).await);
    assert!(is_live(&cp, Kind::Models, direct).await);
    assert!(
        public_model(&cp, &op, id).await["retired_at"].is_null(),
        "the model is not retired"
    );

    // With the grant cleared it goes, proof the grant was the only refusal.
    let cleared = set_grants(&cp, &op, &tenant.to_string(), &[]).await;
    assert_eq!(cleared.status, 200, "{cleared:?}");
    let r = delete_public(&cp, &op, id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(channel_rows(&cp, id).await, 0);
}

#[tokio::test]
async fn a_delete_a_key_selection_holds_is_refused_and_deletes_nothing() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-del-key-8c2").await;
    let (id, _) = create_public(&cp, &op, "pub-del-key-8c2").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, key, id).await;
    // A tenant granted the model, and a key of it that names the model: the
    // key's own selection is the reference, over the tenant's grant.
    let tenant = tenant_granted(&cp, &op, &[id]).await;
    let created = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(op.cookie()),
            Some(json!({
                "label": "del-key-8c2",
                "group_id": support::world::group_id(&cp, support::world::SHARED_GROUP).await,
                "model_ids": [id],
            })),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    // With the grant withdrawn the selection alone still holds it.
    let cleared = set_grants(&cp, &op, &tenant.to_string(), &[]).await;
    assert_eq!(cleared.status, 200, "{cleared:?}");

    let r = delete_public(&cp, &op, id).await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_in_use")),
        "{r:?}"
    );
    assert_eq!(r.body["counts"]["tenants"], 0, "{r:?}");
    assert_eq!(r.body["counts"]["keys"], 1, "{r:?}");
    assert_eq!(r.body["counts"]["channels"], 1, "{r:?}");

    assert_eq!(channel_rows(&cp, id).await, 1, "the mapping is still there");
    assert!(is_live(&cp, Kind::Models, id).await);
    assert!(is_live(&cp, Kind::Models, direct).await);
}

#[tokio::test]
async fn retire_still_refuses_an_enabled_serving_account_where_delete_does_not() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-del-retire-8c2").await;
    let (id, _) = create_public(&cp, &op, "pub-del-retire-8c2").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");

    // The two routes answer the same state differently, which is the whole
    // reason the delete exists: the enabled account refuses a retire only.
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
    assert_eq!(r.body["counts"]["channels"], 1, "{r:?}");
    assert_eq!(channel_rows(&cp, id).await, 1, "the retire deleted nothing");
    assert!(is_live(&cp, Kind::Models, id).await);

    // An already retired model answers `public_model_retired` on both, and
    // a refuse that leaves the row retired is not a double delete.
    let r = delete_public(&cp, &op, id).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = delete_public(&cp, &op, id).await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_retired")),
        "{r:?}"
    );
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
/// nothing. It is refused at `/model_id`, naming the public model to price
/// instead, and no price list offers the direct document.
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
        Some("该模型由公共模型 pub-baseline-8c1 生成，请为公共模型 pub-baseline-8c1 设置基准价"),
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

    // And the baseline list and the charge list offer the public model,
    // never the direct document a baseline cannot be written on.
    for path in ["/admin/pricing/baselines", "/admin/pricing/models"] {
        let r = cp.send("GET", path, Some(op.cookie()), None).await;
        assert_eq!(r.status, 200, "{path}: {r:?}");
        let listed: Vec<&str> = r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|i| i["model_id"].as_str())
            .collect();
        assert!(
            listed.contains(&public.to_string().as_str()),
            "{path}: {r:?}"
        );
        assert!(
            !listed.contains(&direct.to_string().as_str()),
            "{path}: {r:?}"
        );
    }
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
        Some("该模型由公共模型 pub-rate-8c1 生成，请为公共模型 pub-rate-8c1 设置换算率"),
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

/// Writes what a release before the refusal could store: `grants` on
/// `tenant`, and each key's `selection`, by SQL, since the API refuses both
/// now. Then renames the tenant, which re-projects its key documents from
/// those rows, as that release did after the grant.
async fn store_old_grants(
    cp: &TestCp,
    op: &OperatorSession,
    tenant: Uuid,
    grants: &[Uuid],
    selections: &[(Uuid, Vec<Uuid>)],
) {
    sqlx::query("UPDATE tenants SET allowed_model_ids = $2 WHERE id = $1")
        .bind(tenant)
        .bind(grants)
        .execute(cp.db().pool())
        .await
        .expect("store the grants");
    for (key, selection) in selections {
        sqlx::query("UPDATE tenant_keys SET model_ids = $2 WHERE id = $1")
            .bind(key)
            .bind(selection)
            .execute(cp.db().pool())
            .await
            .expect("store the selection");
    }
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
        cp,
        "PATCH",
        &format!("/admin/tenants/{tenant}"),
        op.cookie(),
        version,
        Some(json!({"name": format!("stored-{}", Uuid::new_v4().simple())})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// A key of `tenant` in `shared` minted by an operator, with no selection of
/// its own; its id.
async fn new_key(cp: &TestCp, op: &OperatorSession, tenant: Uuid) -> Uuid {
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(op.cookie()),
            Some(json!({
                "label": format!("stored-{}", Uuid::new_v4().simple()),
                "group_id": support::world::group_id(cp, support::world::SHARED_GROUP).await,
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["key"]["id"].as_str().expect("a key id")).expect("a uuid")
}

/// The model ids key `key`'s live document grants.
async fn key_grants(cp: &TestCp, key: Uuid) -> Vec<Uuid> {
    let doc = live(cp, Kind::ApiKeys, key).await;
    doc["allowed_model_ids"]
        .as_array()
        .unwrap_or_else(|| panic!("no model ids in {doc}"))
        .iter()
        .map(|id| Uuid::parse_str(id.as_str().expect("an id")).expect("a uuid"))
        .collect()
}

/// A key's stored model selection.
async fn selection(cp: &TestCp, key: Uuid) -> Option<Vec<Uuid>> {
    sqlx::query_scalar("SELECT model_ids FROM tenant_keys WHERE id = $1")
        .bind(key)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the key")
}

/// The audit rows the replacement wrote, by tenant.
async fn replacement_rows(cp: &TestCp) -> Vec<(Option<Uuid>, String, Value)> {
    sqlx::query_as(
        "SELECT tenant_id, actor_kind, detail FROM audit_log \
         WHERE action = 'tenant.generated_grants_replaced' ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit log")
}

/// What a pass that touches nothing leaves as it was, for `tenants`: each
/// row's version, each live key's version, the newest `seq` among the
/// tenant's and its keys' documents, and the replacement's audit rows.
async fn untouched(cp: &TestCp, tenants: &[Uuid]) -> (Vec<(i64, Vec<i64>, i64)>, usize) {
    let mut each = Vec::new();
    for tenant in tenants {
        let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the tenant");
        let keys: Vec<(Uuid, i64)> =
            sqlx::query_as("SELECT id, version FROM tenant_keys WHERE tenant_id = $1 ORDER BY id")
                .bind(tenant)
                .fetch_all(cp.db().pool())
                .await
                .expect("read the keys");
        let ids: Vec<Uuid> = keys.iter().map(|(id, _)| *id).collect();
        let seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(max(seq), 0)::bigint FROM gw_resources \
             WHERE managed_by = $1 OR (kind = 'api_keys' AND id = ANY($2))",
        )
        .bind(format!("tenant:{tenant}"))
        .bind(&ids)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the documents");
        each.push((version, keys.into_iter().map(|(_, v)| v).collect(), seq));
    }
    (each, replacement_rows(cp).await.len())
}

/// A call of `key` (of `w`'s tenant) to `model`, as the gateway reports it:
/// its charge's status and amount.
async fn call(
    w: &support::world::World,
    event: &str,
    key: Uuid,
    model: Uuid,
) -> (String, Option<i64>) {
    let mut fields = support::world::tokens_680();
    fields["api_key_id"] = json!(key.to_string());
    if key.to_string() != w.key {
        // A key minted with no member reports no user.
        fields["user_id"] = json!("");
    }
    let r = support::world::deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![support::world::usage(w, event, model, fields)],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    sqlx::query_as("SELECT status, charge_micro_cny FROM pricing_charges_cny WHERE event_id = $1")
        .bind(event)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("the call's charge")
}

/// A grant of a channel's direct document that a release before the
/// refusal stored becomes, at the next boot, a grant of its public model:
/// the grant was of that model through one account. A key that selected the
/// direct document selects the public model, and a call through either key
/// to the one model its document grants is priced. Where the tenant already
/// holds the public model, the direct document simply goes.
///
/// A direct document whose channel an operator removed since (an untick, or
/// a retire of its public model) is withdrawn, not replaced: it reaches
/// nothing, and a key document naming it could not be written. A key's
/// selection of a direct document whose public model its tenant does not
/// hold is withdrawn too, never opened to the public model, and a key whose
/// selection names no direct document is left as it is. Each tenant changed
/// has one audit row naming what was replaced and withdrawn. A later boot
/// touches nothing.
#[tokio::test]
async fn a_stored_grant_of_a_channels_direct_document_becomes_its_public_model() {
    let Some(w) = support::world::world().await else {
        return;
    };
    let (cp, op) = (&w.cp, &w.admin);
    let account = provider_key(cp, op, "acct-gp-5dd").await;
    let (public, _) = create_public(cp, op, "pub-swap-8c1").await;
    let r = put_channel(cp, op, account, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(cp, account, public).await;
    // A second account serving the same public model, unticked later.
    let unticked_account = provider_key(cp, op, "acct-gp-5e0").await;
    let r = put_channel(cp, op, unticked_account, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let unticked = direct_of(cp, unticked_account, public).await;
    // A second public model on the first account, retired later.
    let (gone, _) = create_public(cp, op, "pub-gone-8c1").await;
    let r = put_channel(cp, op, account, gone, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let gone_direct = direct_of(cp, account, gone).await;
    support::world::price(cp, public).await;

    // The world's tenant was granted only direct documents: its key follows
    // every grant, and a second key selected only the first.
    let selecting = new_key(cp, op, w.tenant).await;
    store_old_grants(
        cp,
        op,
        w.tenant,
        &[direct, unticked, gone_direct],
        &[(selecting, vec![direct])],
    )
    .await;
    // Another tenant held the public model beside its direct document, and
    // so did one key; a second key selected the public model alone.
    let other = tenant_granted(cp, op, &[public]).await;
    let other_key = new_key(cp, op, other).await;
    let public_only = new_key(cp, op, other).await;
    store_old_grants(
        cp,
        op,
        other,
        &[public, direct],
        &[
            (other_key, vec![public, direct]),
            (public_only, vec![public]),
        ],
    )
    .await;
    // A third tenant holds nothing, while its key's selection still names
    // the direct document, as one left behind by a grant withdrawn since.
    let narrow = tenant_granted(cp, op, &[]).await;
    let narrow_key = new_key(cp, op, narrow).await;
    store_old_grants(cp, op, narrow, &[], &[(narrow_key, vec![direct])]).await;
    let key = Uuid::parse_str(&w.key).expect("the world's key");
    for k in [key, selecting, other_key] {
        assert!(
            key_grants(cp, k).await.contains(&direct),
            "before the boot, {k}'s document names the direct document"
        );
    }

    // Through the API, as an operator does it: untick the public model on
    // the second account, and retire the second public model once its
    // channel is off. Both remove the channel row and the direct document
    // and leave the grants.
    let r = cp
        .send(
            "DELETE",
            &format!("/admin/provider-keys/{unticked_account}/channel-models/{public}"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    let r = put_channel(cp, op, account, gone, json!({"enabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{gone}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        grants(cp, w.tenant).await,
        vec![direct, unticked, gone_direct]
    );
    let public_only_version: i64 =
        sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id = $1")
            .bind(public_only)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the key");

    let booted = cp.restart().await;
    assert_eq!(grants(&booted, w.tenant).await, vec![public]);
    assert_eq!(grants(&booted, other).await, vec![public]);
    assert_eq!(grants(&booted, narrow).await, Vec::<Uuid>::new());
    assert_eq!(selection(&booted, selecting).await, Some(vec![public]));
    assert_eq!(selection(&booted, other_key).await, Some(vec![public]));
    assert_eq!(selection(&booted, public_only).await, Some(vec![public]));
    assert_eq!(selection(&booted, narrow_key).await, Some(vec![]));
    for k in [key, selecting, other_key, public_only] {
        assert_eq!(key_grants(&booted, k).await, vec![public], "{k}");
    }
    assert_eq!(key_grants(&booted, narrow_key).await, Vec::<Uuid>::new());
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id = $1")
        .bind(public_only)
        .fetch_one(booted.db().pool())
        .await
        .expect("read the key");
    assert_eq!(
        version, public_only_version,
        "a key with nothing to replace is not written"
    );

    // Each key's call to the one model its document grants is priced: the
    // baseline and the group rate are the public model's.
    for (event, k) in [("e-swap-follows", key), ("e-swap-selects", selecting)] {
        let granted = key_grants(&booted, k).await;
        assert_eq!(
            call(&w, event, k, granted[0]).await,
            ("charged".to_string(), Some(680)),
            "{event}"
        );
    }

    // One audit row per tenant changed, by the control plane itself.
    let rows = replacement_rows(&booted).await;
    let pair = json!({"model_id": direct, "public_model_id": public});
    let mut who: Vec<(Option<Uuid>, String)> =
        rows.iter().map(|(t, a, _)| (*t, a.clone())).collect();
    who.sort();
    let mut changed = vec![
        (Some(w.tenant), "system".to_string()),
        (Some(other), "system".to_string()),
        (Some(narrow), "system".to_string()),
    ];
    changed.sort();
    assert_eq!(who, changed, "one row per tenant changed");
    let detail_of = |tenant: Uuid| {
        rows.iter()
            .find(|(t, _, _)| *t == Some(tenant))
            .map(|(_, _, d)| d.clone())
            .unwrap_or_else(|| panic!("no audit row for {tenant}"))
    };
    let mut dead = vec![unticked, gone_direct];
    dead.sort();
    let mut withdrawn: Vec<Uuid> =
        serde_json::from_value(detail_of(w.tenant)["withdrawn"].clone()).expect("withdrawn ids");
    withdrawn.sort();
    assert_eq!(withdrawn, dead);
    assert_eq!(
        (
            detail_of(w.tenant)["replaced"].clone(),
            detail_of(w.tenant)["keys"].clone()
        ),
        (
            json!([pair]),
            json!([{"key_id": selecting, "replaced": [pair], "withdrawn": []}])
        )
    );
    assert_eq!(
        detail_of(other),
        json!({
            "replaced": [pair],
            "withdrawn": [],
            "keys": [{"key_id": other_key, "replaced": [pair], "withdrawn": []}],
        })
    );
    assert_eq!(
        detail_of(narrow),
        json!({
            "replaced": [],
            "withdrawn": [],
            "keys": [{"key_id": narrow_key, "replaced": [], "withdrawn": [direct]}],
        })
    );

    // Idempotent: a later boot finds nothing to replace, and writes no row,
    // no document and no audit entry.
    let before = untouched(&booted, &[w.tenant, other, narrow]).await;
    let again = booted.restart().await;
    assert_eq!(untouched(&again, &[w.tenant, other, narrow]).await, before);
}

/// One tenant held elsewhere holds back no other: the pass replaces the
/// grant of every tenant it can lock and leaves the held one, and the next
/// housekeeping tick replaces that one. Both grants were written after the
/// boot, as an older replica serving the old picker could.
#[tokio::test]
async fn a_held_tenant_holds_back_no_other_and_the_next_tick_replaces_its_grant() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &op, "acct-gp-5df").await;
    let (public, _) = create_public(&cp, &op, "pub-held-8c1").await;
    let r = put_channel(&cp, &op, account, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, account, public).await;
    let a = tenant_granted(&cp, &op, &[]).await;
    let b = tenant_granted(&cp, &op, &[]).await;
    // The pass takes tenants in id order, so the held one comes first.
    let (held, free) = (a.min(b), a.max(b));
    for tenant in [held, free] {
        store_old_grants(&cp, &op, tenant, &[direct], &[]).await;
    }

    let ctx = WriteCtx::of(cp.state());
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(held)
        .execute(&mut *holder)
        .await
        .expect("hold the tenant");
    let done = aisix_control_plane::tenants::replace_generated_grants(cp.db(), &ctx)
        .await
        .expect("the pass");
    holder.rollback().await.expect("release the tenant");
    assert_eq!(
        done,
        aisix_control_plane::tenants::GeneratedGrantsReplaced {
            tenants: 1,
            grants_replaced: 1,
            grants_withdrawn: 0,
            keys_changed: 0,
            deferred: 1,
            failed: 0,
        }
    );
    assert_eq!(grants(&cp, free).await, vec![public]);
    assert_eq!(grants(&cp, held).await, vec![direct]);

    aisix_control_plane::housekeeping::tick(cp.db(), &ctx).await;
    assert_eq!(grants(&cp, held).await, vec![public]);

    // Nothing is left to replace: another pass changes no tenant, key,
    // document or audit row.
    let before = untouched(&cp, &[held, free]).await;
    let done = aisix_control_plane::tenants::replace_generated_grants(cp.db(), &ctx)
        .await
        .expect("the pass");
    assert_eq!(
        done,
        aisix_control_plane::tenants::GeneratedGrantsReplaced::default()
    );
    assert_eq!(untouched(&cp, &[held, free]).await, before);
}

/// The boot's replacement takes each holder's row before the resources
/// lock, as every grant edit does: while a holder is held elsewhere, the boot
/// waits for it holding no resources lock, and replaces the grant once it is
/// free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_boot_replacement_takes_the_tenant_before_the_resources_lock() {
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
        routing: json!({}),
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
        routing: json!({}),
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

/// The generated direct document is marked internal and the routing document
/// is not, because the routing document is the entry a caller addresses.
///
/// This is what the gateway reads: `internal` keeps the direct document off
/// `/v1/models` and out of reach by name, so a key granted by pattern cannot
/// pin one serving account and bypass the public model's routing. A document
/// missing the marker would be listed and callable, and nothing else in the
/// generated shape would say so — an assertion here is the only check that
/// the marker is written at all.
#[test]
fn a_generated_direct_document_is_internal_and_its_routing_document_is_not() {
    let public = channels::PublicModelRow {
        id: Uuid::new_v4(),
        name: "pub-int-9a4".to_string(),
        enabled: true,
        created_by: None,
        created_at: chrono::Utc::now(),
        retired_at: None,
        routing: json!({}),
    };
    let channel = channels::ChannelModelRow {
        id: Uuid::new_v4(),
        provider_key_id: Uuid::new_v4(),
        public_model_id: public.id,
        upstream_model: Some("gpt-4o".to_string()),
        priority: 0,
        weight: 1,
        enabled: true,
        direct_model_id: Uuid::new_v4(),
    };
    let direct = channels::direct_doc(&channel, &public, "acct-int-9a4", "openai");
    assert_eq!(direct["internal"], json!(true), "{direct}");
    let routing = channels::routing_doc(&public, &[channel]);
    assert!(
        routing.get("internal").is_none(),
        "the routing document is the caller-facing entry: {routing}"
    );
}

// ── a selection of upstream models, mapped in one step ──────────────────────

/// `POST /admin/provider-keys/{key}/channel-models` as `who`, mapping
/// `names`.
async fn bulk(cp: &TestCp, who: &OperatorSession, key: Uuid, names: &[&str]) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/provider-keys/{key}/channel-models"),
        Some(who.cookie()),
        Some(json!({ "upstream_models": names })),
    )
    .await
}

/// Each item of a bulk answer as `(upstream_model, public_model_name,
/// outcome)`, in the answer's order.
fn outcomes(r: &Resp) -> Vec<(String, String, String)> {
    r.body["items"]
        .as_array()
        .unwrap_or_else(|| panic!("items: {r:?}"))
        .iter()
        .map(|i| {
            let text = |field: &str| i[field].as_str().unwrap_or_default().to_string();
            (
                text("upstream_model"),
                text("public_model_name"),
                text("outcome"),
            )
        })
        .collect()
}

/// The public model id the answer names for `upstream`.
fn answered_id(r: &Resp, upstream: &str) -> Uuid {
    let item = r.body["items"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["upstream_model"] == upstream))
        .unwrap_or_else(|| panic!("no item for {upstream}: {r:?}"));
    Uuid::parse_str(item["public_model_id"].as_str().expect("an id")).expect("a uuid")
}

/// The public model named `name`, if any row has it.
async fn public_named(cp: &TestCp, name: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT id FROM public_models WHERE name = $1")
        .bind(name)
        .fetch_optional(cp.db().pool())
        .await
        .expect("read the public models")
}

/// Account `key`'s row for `public`: `(upstream_model, priority, weight,
/// enabled, direct_model_id, updated_at)`.
type ChannelSnapshot = (
    Option<String>,
    i32,
    i32,
    bool,
    Uuid,
    chrono::DateTime<chrono::Utc>,
);

async fn channel_row(cp: &TestCp, key: Uuid, public: Uuid) -> ChannelSnapshot {
    sqlx::query_as(
        "SELECT upstream_model, priority, weight, enabled, direct_model_id, updated_at \
         FROM channel_models WHERE provider_key_id = $1 AND public_model_id = $2",
    )
    .bind(key)
    .bind(public)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the channel row")
}

/// What a request that writes nothing leaves as it was: the public models,
/// the channel rows, the newest document `seq` and the audit rows.
async fn footprint(cp: &TestCp) -> (i64, i64, i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM public_models), (SELECT count(*) FROM channel_models), \
           (SELECT COALESCE(max(seq), 0) FROM gw_resources), (SELECT count(*) FROM audit_log)",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read the footprint")
}

/// The paths of a `422`'s details, in order.
fn detail_paths(r: &Resp) -> Vec<String> {
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (422, Some("validation_failed")),
        "{r:?}"
    );
    r.body["details"]
        .as_array()
        .expect("details")
        .iter()
        .filter_map(|d| d["path"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_selection_reuses_or_creates_the_public_model_of_each_name() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-bulk-a1").await;
    let other = provider_key(&cp, &op, "acct-bulk-b1").await;
    // A public model another account already serves, at a higher priority.
    let (reused, _) = create_public(&cp, &op, "bulk-reuse-8c1").await;
    let r = put_channel(&cp, &op, other, reused, json!({"priority": 2})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let other_direct = direct_of(&cp, other, reused).await;
    let publics_before: i64 = sqlx::query_scalar("SELECT count(*) FROM public_models")
        .fetch_one(cp.db().pool())
        .await
        .expect("count the public models");

    let r = bulk(&cp, &op, key, &["bulk-reuse-8c1", "bulk-new-8c1"]).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        outcomes(&r),
        vec![
            (
                "bulk-reuse-8c1".to_string(),
                "bulk-reuse-8c1".to_string(),
                "added".to_string()
            ),
            (
                "bulk-new-8c1".to_string(),
                "bulk-new-8c1".to_string(),
                "created".to_string()
            ),
        ]
    );
    // Reused by id: the existing public model, not a second one.
    assert_eq!(answered_id(&r, "bulk-reuse-8c1"), reused);
    let created = public_named(&cp, "bulk-new-8c1")
        .await
        .expect("the new public model");
    assert_eq!(answered_id(&r, "bulk-new-8c1"), created);
    let publics_after: i64 = sqlx::query_scalar("SELECT count(*) FROM public_models")
        .fetch_one(cp.db().pool())
        .await
        .expect("count the public models");
    assert_eq!(
        publics_after,
        publics_before + 1,
        "one public model created"
    );

    for (public, name) in [(reused, "bulk-reuse-8c1"), (created, "bulk-new-8c1")] {
        // The row a ticked model starts with, naming the upstream model.
        let (upstream, priority, weight, enabled, direct, _) = channel_row(&cp, key, public).await;
        assert_eq!(
            (upstream.as_deref(), priority, weight, enabled),
            (Some(name), 0, 1, true),
            "{name}"
        );
        let doc = live(&cp, Kind::Models, direct).await;
        assert_eq!(doc["display_name"], format!("{name}#acct-bulk-a1"));
        assert_eq!(doc["model_name"], name);
        assert_eq!(doc["provider_key_id"], key.to_string());
        let owner = format!("public:{public}");
        for model in [public, direct] {
            let (_, _, managed_by) = stored(&cp, Kind::Models, model).await.expect("a row");
            assert_eq!(managed_by.as_deref(), Some(owner.as_str()), "{model}");
        }
    }

    // The reused routing document gains this account behind the other one;
    // the created one is served by this account alone.
    let key_reused = direct_of(&cp, key, reused).await;
    assert_eq!(
        live(&cp, Kind::Models, reused).await["routing"]["targets"],
        json!([
            {"model_id": other_direct.to_string(), "priority": 2, "weight": 1},
            {"model_id": key_reused.to_string(), "priority": 0, "weight": 1},
        ])
    );
    let key_created = direct_of(&cp, key, created).await;
    let routing = live(&cp, Kind::Models, created).await;
    assert_eq!(routing["display_name"], "bulk-new-8c1");
    assert_eq!(
        routing["routing"]["targets"],
        json!([{"model_id": key_created.to_string(), "priority": 0, "weight": 1}])
    );

    // Audited as the one-at-a-time routes audit, in the same transaction.
    let rows: Vec<(String, Option<String>, Value)> = sqlx::query_as(
        "SELECT action, target_id, detail FROM audit_log \
         WHERE action IN ('public_model.create', 'channel_model.put') AND detail->>'bulk' = 'true' \
         ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit rows");
    let created_rows: Vec<_> = rows
        .iter()
        .filter(|(action, _, _)| action == "public_model.create")
        .collect();
    assert_eq!(created_rows.len(), 1, "{rows:?}");
    assert_eq!(created_rows[0].1, Some(created.to_string()));
    let mut put_targets: Vec<Option<String>> = rows
        .iter()
        .filter(|(action, _, d)| {
            action == "channel_model.put" && d["provider_key_id"] == key.to_string()
        })
        .map(|(_, target, _)| target.clone())
        .collect();
    put_targets.sort();
    let mut expected = vec![Some(reused.to_string()), Some(created.to_string())];
    expected.sort();
    assert_eq!(put_targets, expected, "{rows:?}");
}

#[tokio::test]
async fn an_invalid_or_colliding_name_refuses_the_whole_selection() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let key = provider_key(&cp, &op, "acct-bulk-a2").await;
    // A hand-made model holding a name, and a retired public model's name.
    let r = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(op.cookie()),
            Some(json!({"doc": {
                "display_name": "bulk-taken-8c1",
                "provider": "openai",
                "model_name": "gpt-4o",
                "provider_key_id": key.to_string(),
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let (retired, _) = create_public(&cp, &op, "bulk-retired-8c1").await;
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{retired}/retire"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let before = footprint(&cp).await;

    // Names a public model cannot have: each named at its own index.
    let r = bulk(
        &cp,
        &op,
        key,
        &["bulk-ok-8c1", "bad#8c1", "bulk-ok2-8c1", "bad*8c1", " "],
    )
    .await;
    assert_eq!(
        detail_paths(&r),
        vec![
            "/upstream_models/1",
            "/upstream_models/3",
            "/upstream_models/4"
        ]
    );
    assert!(
        message_at(&r, "/upstream_models/1").is_some_and(|m| m.contains("bad#8c1")),
        "{r:?}"
    );
    assert!(
        message_at(&r, "/upstream_models/3").is_some_and(|m| m.contains("bad*8c1")),
        "{r:?}"
    );
    assert_eq!(footprint(&cp).await, before, "nothing is written");

    // Names the existing create refuses: a live model of another kind, and a
    // retired public model.
    let r = bulk(
        &cp,
        &op,
        key,
        &["bulk-ok-8c1", "bulk-taken-8c1", "bulk-retired-8c1"],
    )
    .await;
    assert_eq!(
        detail_paths(&r),
        vec!["/upstream_models/1", "/upstream_models/2"]
    );
    assert!(
        message_at(&r, "/upstream_models/1").is_some_and(|m| m.contains("bulk-taken-8c1")),
        "{r:?}"
    );
    assert!(
        message_at(&r, "/upstream_models/2").is_some_and(|m| m.contains("bulk-retired-8c1")),
        "{r:?}"
    );
    assert_eq!(footprint(&cp).await, before, "nothing is written");
    assert_eq!(public_named(&cp, "bulk-ok-8c1").await, None);

    // An account that does not exist, an empty selection, and a viewer.
    let r = bulk(&cp, &op, Uuid::new_v4(), &["bulk-ok-8c1"]).await;
    assert_eq!(detail_paths(&r), vec!["/provider_key_id"]);
    let r = bulk(&cp, &op, key, &[]).await;
    assert_eq!(detail_paths(&r), vec!["/upstream_models"]);
    let r = bulk(&cp, &viewer, key, &["bulk-ok-8c1"]).await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(footprint(&cp).await, before, "nothing is written");

    // Without the offending names the same selection applies.
    let r = bulk(&cp, &op, key, &["bulk-ok-8c1", "bulk-ok2-8c1"]).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(public_named(&cp, "bulk-ok-8c1").await.is_some());
}

#[tokio::test]
async fn rows_the_account_already_maps_are_left_as_they_are() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-bulk-a3").await;
    // The account maps `up-mapped-8c1` under a public model of another
    // name, and serves `bulk-served-8c1` under another upstream name, with
    // that row switched off.
    let (mapped, _) = create_public(&cp, &op, "bulk-mapped-8c1").await;
    let r = put_channel(
        &cp,
        &op,
        key,
        mapped,
        json!({"upstream_model": "up-mapped-8c1", "priority": 5, "weight": 7}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let (served, _) = create_public(&cp, &op, "bulk-served-8c1").await;
    let r = put_channel(
        &cp,
        &op,
        key,
        served,
        json!({"upstream_model": "up-other-8c1", "enabled": false}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let mapped_row = channel_row(&cp, key, mapped).await;
    let served_row = channel_row(&cp, key, served).await;
    let mapped_doc = stored(&cp, Kind::Models, mapped).await;
    let served_doc = stored(&cp, Kind::Models, served).await;

    let r = bulk(
        &cp,
        &op,
        key,
        &["up-mapped-8c1", "bulk-served-8c1", "bulk-fresh-8c1"],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        outcomes(&r),
        vec![
            (
                "up-mapped-8c1".to_string(),
                "bulk-mapped-8c1".to_string(),
                "unchanged".to_string()
            ),
            (
                "bulk-served-8c1".to_string(),
                "bulk-served-8c1".to_string(),
                "unchanged".to_string()
            ),
            (
                "bulk-fresh-8c1".to_string(),
                "bulk-fresh-8c1".to_string(),
                "created".to_string()
            ),
        ]
    );
    assert_eq!(answered_id(&r, "up-mapped-8c1"), mapped);
    assert_eq!(answered_id(&r, "bulk-served-8c1"), served);

    // Neither existing row nor its documents changed, and no public model
    // took the already-mapped upstream name.
    assert_eq!(channel_row(&cp, key, mapped).await, mapped_row);
    assert_eq!(channel_row(&cp, key, served).await, served_row);
    assert_eq!(stored(&cp, Kind::Models, mapped).await, mapped_doc);
    assert_eq!(stored(&cp, Kind::Models, served).await, served_doc);
    assert_eq!(public_named(&cp, "up-mapped-8c1").await, None);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM channel_models WHERE provider_key_id = $1")
            .bind(key)
            .fetch_one(cp.db().pool())
            .await
            .expect("count the account's rows");
    assert_eq!(rows, 3, "one row added, none duplicated");

    // Sent again, every model is already mapped: nothing is written.
    let before = footprint(&cp).await;
    let r = bulk(
        &cp,
        &op,
        key,
        &["up-mapped-8c1", "bulk-served-8c1", "bulk-fresh-8c1"],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(
        outcomes(&r).iter().all(|(_, _, o)| o == "unchanged"),
        "{r:?}"
    );
    assert_eq!(footprint(&cp).await, before, "nothing is written");
}

#[tokio::test]
async fn a_full_selection_holds_the_resources_lock_briefly() {
    // The cap is what keeps one apply's hold on the resources lock short:
    // every other document write queues behind it, and fails once it has
    // waited `lock_timeout`. One over the cap is refused before anything is
    // locked; a full selection applies, well inside half of the default
    // `lock_timeout` (5 s), measured end to end, so the hold is shorter.
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-bulk-a4").await;
    let names: Vec<String> = (0..=channels::MAX_BULK_MODELS)
        .map(|i| format!("bulk-cap-{i:03}"))
        .collect();
    let all: Vec<&str> = names.iter().map(String::as_str).collect();
    let before = footprint(&cp).await;

    let r = bulk(&cp, &op, key, &all).await;
    assert_eq!(detail_paths(&r), vec!["/upstream_models"]);
    assert!(
        message_at(&r, "/upstream_models")
            .is_some_and(|m| m.contains(&channels::MAX_BULK_MODELS.to_string())),
        "{r:?}"
    );
    assert_eq!(footprint(&cp).await, before, "nothing is written");

    let full = &all[..channels::MAX_BULK_MODELS];
    let started = std::time::Instant::now();
    let r = bulk(&cp, &op, key, full).await;
    let took = started.elapsed();
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(outcomes(&r).len(), channels::MAX_BULK_MODELS);
    assert!(outcomes(&r).iter().all(|(_, _, o)| o == "created"), "{r:?}");
    eprintln!(
        "a full selection of {} took {took:?}",
        channels::MAX_BULK_MODELS
    );
    assert!(
        took < std::time::Duration::from_millis(2_500),
        "a full selection took {took:?}"
    );
}

/// Every document a bulk apply writes — direct documents for a reused and a
/// created public model, and both routing documents — loads on the schema
/// a gateway reads it with, and on the strict one the write path applied.
#[tokio::test]
async fn every_document_a_selection_writes_loads_on_the_lenient_schema() {
    use aisix_control_plane::resources::validate::{validate_lenient, validate_strict};

    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-bulk-a5").await;
    let other = provider_key(&cp, &op, "acct-bulk-b5").await;
    let (reused, _) = create_public(&cp, &op, "bulk-schema-8c1").await;
    let r = put_channel(&cp, &op, other, reused, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");

    let r = bulk(&cp, &op, key, &["bulk-schema-8c1", "bulk-schema-new-8c1"]).await;
    assert_eq!(r.status, 200, "{r:?}");
    let created = answered_id(&r, "bulk-schema-new-8c1");

    let docs: Vec<Value> = sqlx::query_scalar(
        "SELECT doc FROM gw_resources WHERE kind = $1 AND NOT deleted \
         AND managed_by = ANY($2) ORDER BY id",
    )
    .bind(Kind::Models.segment())
    .bind(vec![channels::owner(reused), channels::owner(created)])
    .fetch_all(cp.db().pool())
    .await
    .expect("read the generated documents");
    // Two routing documents, two direct ones for the reused model and one
    // for the created one.
    assert_eq!(docs.len(), 5, "{docs:?}");
    for doc in &docs {
        validate_strict(Kind::Models, doc)
            .unwrap_or_else(|e| panic!("strict refused a generated document: {e:?} {doc}"));
        validate_lenient(Kind::Models, doc)
            .unwrap_or_else(|e| panic!("a gateway would skip this document: {e:?} {doc}"));
    }
}

/// A bulk apply's direct documents are as internal as a ticked row's: the
/// price lists offer the public models, never the direct documents, and a
/// tenant is granted the public model, never a direct document.
#[tokio::test]
async fn a_selections_direct_documents_are_never_priced_or_granted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-bulk-a6").await;
    let (reused, _) = create_public(&cp, &op, "bulk-priced-8c1").await;
    let r = bulk(&cp, &op, key, &["bulk-priced-8c1", "bulk-priced-new-8c1"]).await;
    assert_eq!(r.status, 200, "{r:?}");
    let created = answered_id(&r, "bulk-priced-new-8c1");
    let publics = [reused, created];
    let mut directs = Vec::new();
    for public in publics {
        let direct = direct_of(&cp, key, public).await;
        assert!(is_live(&cp, Kind::Models, direct).await, "{direct}");
        directs.push(direct);
    }

    let shared: Uuid = sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = 'shared'")
        .fetch_one(cp.db().pool())
        .await
        .expect("the first-boot group");
    for (path, field) in [
        ("/admin/pricing/baselines".to_string(), "items"),
        ("/admin/pricing/models".to_string(), "items"),
        (format!("/admin/provider-groups/{shared}/rates"), "models"),
    ] {
        let r = cp.send("GET", &path, Some(op.cookie()), None).await;
        assert_eq!(r.status, 200, "{path}: {r:?}");
        let listed: Vec<&str> = r.body[field]
            .as_array()
            .unwrap_or_else(|| panic!("{path}: {r:?}"))
            .iter()
            .filter_map(|i| i["model_id"].as_str())
            .collect();
        for public in publics {
            assert!(
                listed.contains(&public.to_string().as_str()),
                "{path} offers {public}: {r:?}"
            );
        }
        for direct in &directs {
            assert!(
                !listed.contains(&direct.to_string().as_str()),
                "{path} offers the direct document {direct}: {r:?}"
            );
        }
    }

    let tenant = tenant_granted(&cp, &op, &[]).await;
    for direct in &directs {
        let r = set_grants(&cp, &op, &tenant.to_string(), &[*direct]).await;
        assert_422_at(&r, "/allowed_model_ids/0");
    }
    assert_eq!(grants(&cp, tenant).await, Vec::<Uuid>::new());
    let r = set_grants(&cp, &op, &tenant.to_string(), &publics).await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// Waits until a session waits on the transaction of backend `holder`; fails
/// after 10 s.
async fn until_blocked(cp: &TestCp, holder: i32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: Option<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_locks l WHERE NOT l.granted AND l.locktype = 'transactionid' \
             AND $1 = ANY(pg_blocking_pids(l.pid)) LIMIT 1",
        )
        .bind(holder)
        .fetch_optional(cp.db().pool())
        .await
        .expect("read pg_locks");
        if waiting.is_some() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no session waits on the public model's holder"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Whether some session holds `RESOURCES_LOCK_KEY`, tried from a session of
/// the test's own.
async fn resources_lock_held(cp: &TestCp) -> bool {
    let mut probe = cp.db().pool().begin().await.expect("begin");
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .fetch_one(&mut *probe)
        .await
        .expect("try the resources lock");
    probe.rollback().await.expect("end the probe");
    !free
}

/// A bulk apply takes the public models it reuses before the resources lock:
/// while it waits for one held elsewhere, the resources lock is still free.
/// The reverse order would hold the resources lock while it waited, and a
/// channel edit holding that row would then wait for the resources lock
/// behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_selection_takes_its_public_models_before_the_resources_lock() {
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-bulk-a7").await;
    let (reused, _) = create_public(&cp, &op, "bulk-order-8c1").await;

    let mut holder = cp.db().pool().begin().await.expect("begin");
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .expect("this session's pid");
    sqlx::query("SELECT 1 FROM public_models WHERE id = $1 FOR UPDATE")
        .bind(reused)
        .execute(&mut *holder)
        .await
        .expect("hold the public model");

    let url = cp.url(&format!("/admin/provider-keys/{key}/channel-models"));
    let cookie = op.cookie().to_string();
    let sent = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an HTTP client")
            .post(url)
            .header(reqwest::header::COOKIE, cookie)
            .json(&json!({"upstream_models": ["bulk-order-8c1", "bulk-order-new-8c1"]}))
            .send()
            .await
            .expect("send the request")
            .status()
            .as_u16()
    });
    until_blocked(&cp, holder_pid).await;
    let resources_free = !resources_lock_held(&cp).await;
    holder.commit().await.expect("release the public model");
    assert_eq!(sent.await.expect("join the request"), 200);
    assert!(
        resources_free,
        "the apply took the resources lock before the public model's: the lock order is inverted"
    );
}

// ── the routing settings a public model carries ─────────────────────────────

/// `PATCH /admin/public-models/{id}` as `who`, with `body`.
async fn patch_public(cp: &TestCp, who: &OperatorSession, id: Uuid, body: Value) -> Resp {
    cp.send(
        "PATCH",
        &format!("{PUBLICS}/{id}"),
        Some(who.cookie()),
        Some(body),
    )
    .await
}

/// `public_models.routing` as stored, for a row that must exist.
async fn stored_routing(cp: &TestCp, id: Uuid) -> Value {
    sqlx::query_scalar("SELECT routing FROM public_models WHERE id = $1")
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the routing column")
}

/// A row written before `0037` reads as `{}` and still loads, and the document
/// generated from it is byte-identical to the one built before the settings
/// were stored at all: `round_robin` over exactly these targets, nothing else.
///
/// The insert omits the column, which is what an upgrade leaves behind and is
/// the only way to reach the pre-`0037` shape from a migrated database.
#[tokio::test]
async fn a_row_written_before_the_routing_column_reads_as_empty_and_generates_todays_document() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-legacy-8c1").await;
    let (id, _) = create_public(&cp, &op, "pub-legacy-8c1").await;
    let r = put_channel(&cp, &op, key, id, json!({"priority": 2, "weight": 3})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct_id: Uuid = sqlx::query_scalar(
        "SELECT direct_model_id FROM channel_models WHERE provider_key_id = $1 AND public_model_id = $2",
    )
    .bind(key)
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the channel row");

    // The shape an upgrade leaves: the column defaults, nothing written.
    sqlx::query("UPDATE public_models SET routing = DEFAULT WHERE id = $1")
        .bind(id)
        .execute(cp.db().pool())
        .await
        .expect("reset the column to the pre-0037 shape");
    assert_eq!(stored_routing(&cp, id).await, json!({}));

    // Same path a channel edit takes, so the generated document is rebuilt
    // from the row as it now stands rather than read from the earlier write.
    let ctx = WriteCtx::of(cp.state());
    cp.db()
        .tx(move |conn| {
            async move {
                channels::regenerate(conn, &ctx, id, &[]).await?;
                Ok::<_, aisix_control_plane::error::ApiError>(())
            }
            .boxed()
        })
        .await
        .expect("regenerate");

    assert_eq!(
        live(&cp, Kind::Models, id).await,
        json!({
            "display_name": "pub-legacy-8c1",
            "routing": {
                "strategy": "round_robin",
                "targets": [{"model_id": direct_id.to_string(), "priority": 2, "weight": 3}],
            },
        })
    );
    // And the view says the model sets nothing of its own.
    assert_eq!(public_model(&cp, &op, id).await["routing"], json!({}));
}

/// The settings an operator saves are written to the row, carried by the
/// regenerated document, and survive the next account change — the whole
/// point of storing them: before this, the strategy was hard-coded, so a tick
/// on any account serving the model rewrote `round_robin` over whatever the
/// operator had chosen.
#[tokio::test]
async fn saved_routing_settings_reach_the_document_and_survive_an_account_change() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-settings-8c1").await;
    let (id, _) = create_public(&cp, &op, "pub-settings-8c1").await;
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

    let settings = json!({
        "strategy": "consistent_hash",
        "retries": 2,
        "retry_on_429": true,
        "fallback_on_statuses": [408, 429],
        "when_all_unavailable": "try_anyway",
        "hash_on": [{"type": "header", "name": "x-tenant"}],
    });
    let r = patch_public(&cp, &op, id, json!({"routing": settings})).await;
    assert_eq!(r.status, 200, "{r:?}");
    // The answer is the view, so a form re-renders from what was stored.
    assert_eq!(r.body["routing"], settings);

    let doc = live(&cp, Kind::Models, id).await;
    assert_eq!(doc["routing"]["strategy"], "consistent_hash");
    assert_eq!(doc["routing"]["retries"], 2);
    assert_eq!(doc["routing"]["retry_on_429"], true);
    assert_eq!(doc["routing"]["fallback_on_statuses"], json!([408, 429]));
    assert_eq!(doc["routing"]["when_all_unavailable"], "try_anyway");
    assert_eq!(
        doc["routing"]["hash_on"],
        json!([{"type": "header", "name": "x-tenant"}])
    );
    // The targets stay generated: the settings did not replace them.
    assert_eq!(
        doc["routing"]["targets"],
        json!([{"model_id": direct_id.to_string(), "priority": 0, "weight": 1}])
    );

    // An account change on another model served by this one regenerates its
    // document. The settings must come back unchanged.
    let r = put_channel(&cp, &op, key, id, json!({"priority": 5})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let doc = live(&cp, Kind::Models, id).await;
    assert_eq!(
        doc["routing"]["strategy"], "consistent_hash",
        "a channel edit reverted the operator's strategy: {doc}"
    );
    assert_eq!(doc["routing"]["when_all_unavailable"], "try_anyway");
    assert_eq!(
        doc["routing"]["targets"],
        json!([{"model_id": direct_id.to_string(), "priority": 5, "weight": 1}]),
        "the ticks still decide the targets"
    );

    // Unticking the account is a channel change too, and keeps them as well.
    let r = cp
        .send(
            "DELETE",
            &format!("/admin/provider-keys/{key}/channel-models/{id}"),
            Some(op.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    let doc = live(&cp, Kind::Models, id).await;
    assert_eq!(doc["routing"]["strategy"], "consistent_hash");
    assert_eq!(doc["routing"]["targets"], json!([]));
}

/// Only what an operator can actually choose is stored: a setting equal to the
/// gateway's default is dropped, so a default that moves in a later release
/// reaches a model that never overrode it.
#[tokio::test]
async fn a_setting_equal_to_the_gateway_default_is_not_stored() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (id, _) = create_public(&cp, &op, "pub-defaults-8c1").await;

    let r = patch_public(
        &cp,
        &op,
        id,
        json!({"routing": {
            "strategy": "round_robin",
            "retry_on_429": false,
            "when_all_unavailable": "fail",
            "fallback_on_statuses": [],
        }}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(stored_routing(&cp, id).await, json!({}));

    // `retries` has no gateway default (it defers to the deployment's
    // `upstream.retries`), so a written `0` is an operator's choice and stays.
    let r = patch_public(&cp, &op, id, json!({"routing": {"retries": 0}})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(stored_routing(&cp, id).await, json!({"retries": 0}));
    // A body that sets nothing clears the row rather than leaving it.
    let r = patch_public(&cp, &op, id, json!({"routing": {}})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(stored_routing(&cp, id).await, json!({}));
}

/// The targets are generated from the accounts, never set through this route,
/// and a key the gateway's routing block does not define is refused by name.
#[tokio::test]
async fn a_targets_key_or_an_unknown_setting_is_refused_by_name() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-refuse-8c1").await;
    let (id, _) = create_public(&cp, &op, "pub-refuse-8c1").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let before = live(&cp, Kind::Models, id).await;
    let direct_id: Uuid = sqlx::query_scalar(
        "SELECT direct_model_id FROM channel_models WHERE provider_key_id = $1 AND public_model_id = $2",
    )
    .bind(key)
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the channel row");

    let r = patch_public(&cp, &op, id, json!({"routing": {"targets": []}})).await;
    assert_422_at(&r, "/routing/targets");

    let r = patch_public(&cp, &op, id, json!({"routing": {"sticky": true}})).await;
    assert_422_at(&r, "/routing/sticky");

    // A strategy the gateway does not have, and a status outside the range
    // the schema allows, are refused at their own paths.
    let r = patch_public(&cp, &op, id, json!({"routing": {"strategy": "random"}})).await;
    assert_422_at(&r, "/routing/strategy");
    let r = patch_public(
        &cp,
        &op,
        id,
        json!({"routing": {"fallback_on_statuses": [200]}}),
    )
    .await;
    assert_422_at(&r, "/routing/fallback_on_statuses/0");

    // Nothing was written or regenerated by any refusal.
    assert_eq!(stored_routing(&cp, id).await, json!({}));
    assert_eq!(live(&cp, Kind::Models, id).await, before);
    assert!(live(&cp, Kind::Models, id).await["routing"]["targets"]
        .as_array()
        .is_some_and(|t| t.len() == 1 && t[0]["model_id"] == direct_id.to_string()));

    // A viewer may read but not set them.
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let r = patch_public(&cp, &viewer, id, json!({"routing": {}})).await;
    assert_eq!(r.status, 403, "{r:?}");
}

/// A retired public model takes no settings: nothing would read them.
#[tokio::test]
async fn a_retired_public_model_refuses_settings() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (id, _) = create_public(&cp, &op, "pub-retired-8c1").await;
    let r = cp
        .send(
            "POST",
            &format!("{PUBLICS}/{id}/delete"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = patch_public(&cp, &op, id, json!({"routing": {"retries": 1}})).await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_retired")),
        "{r:?}"
    );
}

/// The internal tenant's id.
async fn internal_tenant(cp: &TestCp) -> Uuid {
    sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(cp.db().pool())
        .await
        .expect("the internal tenant")
}

/// `POST /admin/tenants/{tenant}/keys` with `body`, which must answer 201;
/// the key's id.
async fn mint(cp: &TestCp, op: &OperatorSession, tenant: Uuid, body: Value) -> Uuid {
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(op.cookie()),
            Some(body),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["key"]["id"].as_str().expect("a key id")).expect("a uuid")
}

/// A customer tenant as an operator creates it, which leaves it on every
/// public model (the `all` scope); its id.
async fn all_scope_tenant(cp: &TestCp, op: &OperatorSession) -> Uuid {
    let created = cp
        .send(
            "POST",
            "/admin/tenants",
            Some(op.cookie()),
            Some(json!({"name": format!("chan-all-{}", Uuid::new_v4().simple())})),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    assert_eq!(created.body["model_scope"], "all", "{created:?}");
    Uuid::parse_str(created.body["id"].as_str().expect("a tenant id")).expect("a uuid")
}

/// Who can call a public model is not what holds it from being removed: an
/// all-scope tenant's keys reach every public model and name no reference.
/// The view says both — `reachable_*` counts every tenant whose scope reaches
/// the model and every live key whose grant does, the platform's own name
/// patterns included — while `tenants`/`keys`, which the removal refusal
/// reads, stay as they were.
#[tokio::test]
async fn an_all_scope_tenant_and_its_keys_reach_a_public_model_without_holding_it() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &op, "acct-reach-5d2").await;
    let (public, _) = create_public(&cp, &op, "pub-reach-8c1").await;
    let (other, _) = create_public(&cp, &op, "pub-elsewhere-8c1").await;
    for id in [public, other] {
        let r = put_channel(&cp, &op, account, id, json!({})).await;
        assert_eq!(r.status, 200, "{r:?}");
    }
    let shared = support::world::group_id(&cp, support::world::SHARED_GROUP).await;

    // An all-scope customer: one key with no selection (every public model),
    // one selecting the model, one selecting only the other model.
    let customer = all_scope_tenant(&cp, &op).await;
    mint(
        &cp,
        &op,
        customer,
        json!({"label": "reach-all-8c1", "group_id": shared}),
    )
    .await;
    mint(
        &cp,
        &op,
        customer,
        json!({"label": "reach-one-8c1", "group_id": shared, "model_ids": [public]}),
    )
    .await;
    mint(
        &cp,
        &op,
        customer,
        json!({"label": "reach-other-8c1", "group_id": shared, "model_ids": [other]}),
    )
    .await;
    // The platform's own keys, by name pattern: one that matches the model,
    // one that does not.
    let internal = internal_tenant(&cp).await;
    mint(
        &cp,
        &op,
        internal,
        json!({"label": "ops-reach-8c1", "allowed_models": ["pub-reach-*"], "group_id": shared}),
    )
    .await;
    mint(
        &cp,
        &op,
        internal,
        json!({"label": "ops-miss-8c1", "allowed_models": ["claude-*"], "group_id": shared}),
    )
    .await;

    let view = public_model(&cp, &op, public).await;
    assert_eq!(
        (
            view["reachable_tenants"].clone(),
            view["reachable_keys"].clone()
        ),
        (json!(1), json!(3)),
        "the customer, and its two keys that reach the model plus the matching pattern: {view}"
    );
    // The references a removal is refused on are unchanged: none of these
    // names the model in a selected scope.
    assert_eq!(
        (view["tenants"].clone(), view["keys"].clone()),
        (json!(0), json!(0)),
        "{view}"
    );
    // The list answers the same counts.
    let r = cp.send("GET", PUBLICS, Some(op.cookie()), None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let listed = r.body["items"]
        .as_array()
        .and_then(|items| items.iter().find(|m| m["id"] == public.to_string()))
        .cloned()
        .expect("the model is listed");
    assert_eq!(
        (
            listed["reachable_tenants"].clone(),
            listed["reachable_keys"].clone()
        ),
        (json!(1), json!(3)),
        "{listed}"
    );
    let other_view = public_model(&cp, &op, other).await;
    assert_eq!(
        (
            other_view["reachable_tenants"].clone(),
            other_view["reachable_keys"].clone()
        ),
        (json!(1), json!(2)),
        "the key with no selection and the one selecting it: {other_view}"
    );

    // Nothing refuses the delete, and once retired nothing reaches it.
    let r = delete_public(&cp, &op, public).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        (
            r.body["reachable_tenants"].clone(),
            r.body["reachable_keys"].clone()
        ),
        (json!(0), json!(0)),
        "{r:?}"
    );
}

/// `POST /admin/public-models/{id}/restore` as `who`.
async fn restore_public(cp: &TestCp, who: &OperatorSession, id: Uuid) -> Resp {
    cp.send(
        "POST",
        &format!("{PUBLICS}/{id}/restore"),
        Some(who.cookie()),
        Some(json!({})),
    )
    .await
}

/// A retired public model comes back under its own id: live, enabled, its
/// routing settings kept, its routing document regenerated with no target,
/// and in every all-scope key's grant again — as a public model that just
/// became callable is. Mapping an account by name reuses it, and a baseline
/// can be written on it again.
#[tokio::test]
async fn a_retired_public_model_is_restored_under_its_own_id() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &op, "acct-restore-5d2").await;
    let (public, _) = create_public(&cp, &op, "pub-restore-8c1").await;
    let r = put_channel(&cp, &op, account, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = patch_public(
        &cp,
        &op,
        public,
        json!({"routing": {"strategy": "failover", "retries": 2}}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let shared = support::world::group_id(&cp, support::world::SHARED_GROUP).await;
    let customer = all_scope_tenant(&cp, &op).await;
    let key = mint(
        &cp,
        &op,
        customer,
        json!({"label": "restore-8c1", "group_id": shared}),
    )
    .await;
    assert!(key_grants(&cp, key).await.contains(&public));

    let r = delete_public(&cp, &op, public).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(!is_live(&cp, Kind::Models, public).await);
    assert!(!key_grants(&cp, key).await.contains(&public));
    // Retired, the model takes no price.
    let baseline = json!({
        "model_id": public,
        "effective_from": chrono::Utc::now(),
        "card": {"default": {"base": {"input": "1", "output": "2"}}},
        "note": "restore",
    });
    let r = cp
        .send(
            "POST",
            "/admin/pricing/baselines",
            Some(op.cookie()),
            Some(baseline.clone()),
        )
        .await;
    assert_422_at(&r, "/model_id");

    let r = restore_public(&cp, &op, public).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["id"], public.to_string());
    assert!(r.body["retired_at"].is_null(), "{r:?}");
    assert_eq!(r.body["enabled"], true, "{r:?}");
    assert_eq!(r.body["served_by"], json!([]), "{r:?}");
    assert_eq!(
        r.body["routing"],
        json!({"strategy": "failover", "retries": 2}),
        "the settings the model had are kept: {r:?}"
    );
    assert_eq!(r.body["reachable_keys"], 1, "{r:?}");
    let doc = live(&cp, Kind::Models, public).await;
    assert_eq!(doc["display_name"], "pub-restore-8c1");
    assert_eq!(doc["routing"]["targets"], json!([]), "{doc}");
    assert_eq!(doc["routing"]["strategy"], "failover", "{doc}");
    assert!(
        key_grants(&cp, key).await.contains(&public),
        "the all-scope key reaches the restored model again"
    );
    assert_eq!(
        audit_detail(&cp, "public_model.restore", public).await,
        json!({"name": "pub-restore-8c1"})
    );

    // The account maps it again by name, onto the same id.
    let r = bulk(&cp, &op, account, &["pub-restore-8c1"]).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        outcomes(&r),
        vec![(
            "pub-restore-8c1".to_string(),
            "pub-restore-8c1".to_string(),
            "added".to_string()
        )]
    );
    assert_eq!(answered_id(&r, "pub-restore-8c1"), public);
    let doc = live(&cp, Kind::Models, public).await;
    assert_eq!(
        doc["routing"]["targets"].as_array().map(Vec::len),
        Some(1),
        "{doc}"
    );

    // And it takes a baseline again.
    let r = cp
        .send(
            "POST",
            "/admin/pricing/baselines",
            Some(op.cookie()),
            Some(baseline),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
}

/// A restore is refused for a model that is not retired, for a name a live
/// model took after the retire freed it, and for an id that names nothing;
/// each refusal leaves the row as it was.
#[tokio::test]
async fn a_restore_is_refused_unless_the_model_is_retired_and_its_name_free() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (live_model, _) = create_public(&cp, &op, "pub-live-8c1").await;
    let r = restore_public(&cp, &op, live_model).await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_not_retired")),
        "{r:?}"
    );

    let r = restore_public(&cp, &op, Uuid::new_v4()).await;
    assert_eq!(r.status, 404, "{r:?}");

    let account = provider_key(&cp, &op, "acct-held-5d2").await;
    let (public, _) = create_public(&cp, &op, "pub-held-8c1").await;
    let r = put_channel(&cp, &op, account, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = delete_public(&cp, &op, public).await;
    assert_eq!(r.status, 200, "{r:?}");
    // A hand-made model takes the name the retire freed.
    let r = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(op.cookie()),
            Some(json!({"doc": {
                "display_name": "pub-held-8c1",
                "provider": "openai",
                "model_name": "gpt-4o-mini",
                "provider_key_id": account,
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let r = restore_public(&cp, &op, public).await;
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (409, Some("public_model_name_held")),
        "{r:?}"
    );
    let view = public_model(&cp, &op, public).await;
    assert!(!view["retired_at"].is_null(), "still retired: {view}");
    assert!(!is_live(&cp, Kind::Models, public).await);

    // A viewer cannot restore.
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let r = restore_public(&cp, &viewer, public).await;
    assert_eq!(r.status, 403, "{r:?}");
}

/// `least_cost` ranks targets by their price, and a public model's generated
/// targets carry none, so the strategy is refused at its field rather than
/// stored and silently ordering by the target list.
#[tokio::test]
async fn a_public_model_cannot_route_by_least_cost() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (public, _) = create_public(&cp, &op, "pub-cost-8c1").await;
    let r = patch_public(
        &cp,
        &op,
        public,
        json!({"routing": {"strategy": "least_cost"}}),
    )
    .await;
    assert_422_at(&r, "/routing/strategy");
    assert_eq!(stored_routing(&cp, public).await, json!({}));
    // The other metric strategies need no price, and are kept.
    let r = patch_public(
        &cp,
        &op,
        public,
        json!({"routing": {"strategy": "least_latency"}}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        stored_routing(&cp, public).await,
        json!({"strategy": "least_latency"})
    );
}

/// Backends of `cp`'s database waiting for a lock.
async fn lock_waiters(cp: &TestCp) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read pg_stat_activity")
}

/// Returns once `n` backends wait for a lock; fails after 60 s.
async fn until_lock_waiters(cp: &TestCp, n: i64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while lock_waiters(cp).await < n {
        assert!(
            std::time::Instant::now() < deadline,
            "{n} lock waiters never appeared"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// A transaction that holds back every insert into `audit_log` until it
/// ends: a change blocks on its audit row after doing everything else.
async fn hold_audit_inserts(cp: &TestCp) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("LOCK TABLE audit_log IN SHARE MODE")
        .execute(&mut *holder)
        .await
        .expect("hold audit_log");
    holder
}

/// `DELETE /admin/resources/provider_keys/{key}` at its current version.
async fn delete_provider_key(cp: &TestCp, who: &OperatorSession, key: Uuid) -> Resp {
    let current = cp
        .send(
            "GET",
            &format!("/admin/resources/provider_keys/{key}"),
            Some(who.cookie()),
            None,
        )
        .await;
    let version = current.body["version"].as_i64().expect("a version");
    send_conditional(
        cp,
        "DELETE",
        &format!("/admin/resources/provider_keys/{key}"),
        who.cookie(),
        version,
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disabled_mapping_written_while_its_account_is_deleted_is_refused() {
    // The delete counts the account's channel rows and commits first; the
    // channel edit read the account before that commit. Once the delete has
    // committed, the edit must not leave a row naming an account that is
    // gone.
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let cp = std::sync::Arc::new(cp);
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-race-del-5d1").await;
    let (id, _) = create_public(&cp, &op, "pub-race-del-8c1").await;
    let holder = hold_audit_inserts(&cp).await;

    let delete = tokio::spawn({
        let (cp, op) = (cp.clone(), op.clone());
        async move { delete_provider_key(&cp, &op, key).await }
    });
    until_lock_waiters(&cp, 1).await;
    let put = tokio::spawn({
        let (cp, op) = (cp.clone(), op.clone());
        async move { put_channel(&cp, &op, key, id, json!({"enabled": false})).await }
    });
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release audit_log");

    let delete = delete.await.expect("join");
    assert_eq!(delete.status, 200, "{delete:?}");
    let put = put.await.expect("join");
    assert_422_at(&put, "/provider_key_id");
    assert_eq!(channel_rows(&cp, id).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_account_deleted_while_a_disabled_mapping_is_written_is_refused() {
    // The channel edit writes its row first and commits; the delete was
    // sent while the row was uncommitted. Once the row has committed, the
    // delete must count it and refuse.
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let cp = std::sync::Arc::new(cp);
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-race-put-5d1").await;
    let (id, _) = create_public(&cp, &op, "pub-race-put-8c1").await;
    let version = cp
        .send(
            "GET",
            &format!("/admin/resources/provider_keys/{key}"),
            Some(op.cookie()),
            None,
        )
        .await
        .body["version"]
        .as_i64()
        .expect("a version");
    let holder = hold_audit_inserts(&cp).await;

    let put = tokio::spawn({
        let (cp, op) = (cp.clone(), op.clone());
        async move { put_channel(&cp, &op, key, id, json!({"enabled": false})).await }
    });
    until_lock_waiters(&cp, 1).await;
    let delete = tokio::spawn({
        let (cp, cookie) = (cp.clone(), op.cookie().to_string());
        async move {
            send_conditional(
                &cp,
                "DELETE",
                &format!("/admin/resources/provider_keys/{key}"),
                &cookie,
                version,
                None,
            )
            .await
        }
    });
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release audit_log");

    let put = put.await.expect("join");
    assert_eq!(put.status, 200, "{put:?}");
    let delete = delete.await.expect("join");
    assert_eq!(
        (delete.status, delete.body["code"].as_str()),
        (409, Some("in_use")),
        "{delete:?}"
    );
    assert!(is_live(&cp, Kind::ProviderKeys, key).await);
    assert_eq!(channel_rows(&cp, id).await, 1);
}

/// `PUT /admin/resources/provider_keys/{key}` renaming the account to
/// `name`, at its current version; the answer.
async fn rename_account(cp: &TestCp, who: &OperatorSession, key: Uuid, name: &str) -> Resp {
    let current = cp
        .send(
            "GET",
            &format!("/admin/resources/provider_keys/{key}"),
            Some(who.cookie()),
            None,
        )
        .await;
    assert_eq!(current.status, 200, "{current:?}");
    let version = current.body["version"].as_i64().expect("a version");
    let mut doc = current.body["doc"].clone();
    doc["display_name"] = json!(name);
    send_conditional(
        cp,
        "PUT",
        &format!("/admin/resources/provider_keys/{key}"),
        who.cookie(),
        version,
        Some(json!({"doc": doc})),
    )
    .await
}

#[tokio::test]
async fn a_renamed_accounts_direct_documents_follow_it_and_free_the_old_name() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let old_name = "acct-rename-old-5d1";
    let new_name = "acct-rename-new-5d1";
    let a = provider_key(&cp, &op, old_name).await;
    let (id, _) = create_public(&cp, &op, "pub-rename-8c1").await;
    let r = put_channel(&cp, &op, a, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let a_direct = direct_of(&cp, a, id).await;

    let r = rename_account(&cp, &op, a, new_name).await;
    assert_eq!(r.status, 200, "{r:?}");
    let doc = live(&cp, Kind::Models, a_direct).await;
    assert_eq!(
        doc["display_name"],
        json!(format!("pub-rename-8c1#{new_name}")),
        "{doc}"
    );

    // Another account takes the name given up, and serves the same model
    // ahead of the renamed one.
    let b = provider_key(&cp, &op, old_name).await;
    let r = put_channel(&cp, &op, b, id, json!({"priority": 10})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let b_direct = direct_of(&cp, b, id).await;
    let doc = live(&cp, Kind::Models, b_direct).await;
    assert_eq!(
        doc["display_name"],
        json!(format!("pub-rename-8c1#{old_name}")),
        "{doc}"
    );
    assert!(is_live(&cp, Kind::Models, a_direct).await);
}

#[tokio::test]
async fn a_regeneration_frees_the_name_a_stale_direct_document_holds_before_reusing_it() {
    // A direct document renamed before this release still carries the old
    // account name. The account that takes that name later, ordered ahead
    // of the renamed one, must still be served: the regeneration rewrites
    // the stale document before it creates the new one.
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let old_name = "acct-stale-old-5d1";
    let new_name = "acct-stale-new-5d1";
    let a = provider_key(&cp, &op, old_name).await;
    let (id, _) = create_public(&cp, &op, "pub-stale-8c1").await;
    let r = put_channel(&cp, &op, a, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let a_direct = direct_of(&cp, a, id).await;
    let r = rename_account(&cp, &op, a, new_name).await;
    assert_eq!(r.status, 200, "{r:?}");
    // What an older release left behind: the direct document kept the name.
    sqlx::query(
        "UPDATE gw_resources SET doc = jsonb_set(doc, '{display_name}', to_jsonb($2::text)), \
           identity = $2 \
         WHERE kind = 'models' AND id = $1",
    )
    .bind(a_direct)
    .bind(format!("pub-stale-8c1#{old_name}"))
    .execute(cp.db().pool())
    .await
    .expect("leave the stale name");

    let b = provider_key(&cp, &op, old_name).await;
    let r = put_channel(&cp, &op, b, id, json!({"priority": 10})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let doc = live(&cp, Kind::Models, a_direct).await;
    assert_eq!(
        doc["display_name"],
        json!(format!("pub-stale-8c1#{new_name}")),
        "{doc}"
    );
    let doc = live(&cp, Kind::Models, direct_of(&cp, b, id).await).await;
    assert_eq!(
        doc["display_name"],
        json!(format!("pub-stale-8c1#{old_name}")),
        "{doc}"
    );
}

/// A tenant whose stored grants name nothing but the direct document of a
/// channel removed since — the last one an older release let it hold — is
/// still found by the next pass, which withdraws the grant and the key
/// selection naming it: they reach nothing, and a key document naming the
/// direct document could not be written again. Nothing is granted in its
/// place.
#[tokio::test]
async fn a_grant_of_only_removed_channels_is_withdrawn_at_the_next_boot() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &op, "acct-dead-5e1").await;
    let (public, _) = create_public(&cp, &op, "pub-dead-8c1").await;
    let r = put_channel(&cp, &op, account, public, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, account, public).await;
    let tenant = tenant_granted(&cp, &op, &[]).await;
    let follows = new_key(&cp, &op, tenant).await;
    let selecting = new_key(&cp, &op, tenant).await;
    store_old_grants(&cp, &op, tenant, &[direct], &[(selecting, vec![direct])]).await;
    assert_eq!(key_grants(&cp, follows).await, vec![direct]);

    // The operator unticks the account's last channel of the model.
    let r = cp
        .send(
            "DELETE",
            &format!("/admin/provider-keys/{account}/channel-models/{public}"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    assert!(!is_live(&cp, Kind::Models, direct).await);

    let booted = cp.restart().await;
    assert_eq!(grants(&booted, tenant).await, Vec::<Uuid>::new());
    assert_eq!(selection(&booted, selecting).await, Some(vec![]));
    for k in [follows, selecting] {
        assert_eq!(key_grants(&booted, k).await, Vec::<Uuid>::new(), "{k}");
    }
    let rows = replacement_rows(&booted).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].2,
        json!({
            "replaced": [],
            "withdrawn": [direct],
            "keys": [{"key_id": selecting, "replaced": [], "withdrawn": [direct]}],
        })
    );
    // An ordinary edit re-projects the keys again.
    let current = booted
        .send(
            "GET",
            &format!("/admin/tenants/{tenant}"),
            Some(op.cookie()),
            None,
        )
        .await;
    let version = current.body["version"].as_i64().expect("a version");
    let r = send_conditional(
        &booted,
        "PATCH",
        &format!("/admin/tenants/{tenant}"),
        op.cookie(),
        version,
        Some(json!({"name": "dead-renamed-5e1"})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// Writes `doc` as public model `id`'s routing document through
/// `write_resource`, the way a control plane from before `0037` regenerates
/// it on a channel edit; the outcome.
async fn write_routing_doc(
    cp: &TestCp,
    id: Uuid,
    doc: Value,
) -> Result<(), aisix_control_plane::error::ApiError> {
    use aisix_control_plane::resources::write::{write_resource, ResourceWrite, WriteOp};
    let ctx = WriteCtx::of(cp.state());
    cp.db()
        .tx(move |conn| {
            async move {
                let owner = channels::owner(id);
                write_resource(
                    conn,
                    &ctx,
                    ResourceWrite {
                        kind: Kind::Models,
                        id,
                        op: WriteOp::Replace(&doc),
                        expected_version: None,
                        managed_by: Some(&owner),
                    },
                )
                .await?;
                Ok::<_, aisix_control_plane::error::ApiError>(())
            }
            .boxed()
        })
        .await
}

/// A control plane from before `0037` regenerates a public model's routing
/// document from its own columns: the default strategy over the targets,
/// without the settings the operator stored since. On a model that stores
/// settings that write is refused, so the gateway never routes the model as
/// if they were not set; on one that stores none it is what every release
/// writes, and still goes through. This release's own writes carry every
/// setting, each with its stored value.
#[tokio::test]
async fn an_older_writer_cannot_drop_a_public_models_stored_routing() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-fence-8c1").await;
    let (id, _) = create_public(&cp, &op, "pub-fence-8c1").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct = direct_of(&cp, key, id).await;
    let targets = json!([{"model_id": direct.to_string(), "priority": 0, "weight": 1}]);
    let older = json!({
        "display_name": "pub-fence-8c1",
        "routing": {"strategy": "round_robin", "targets": targets},
    });

    // Stored nothing: the older shape is today's, and it is written.
    write_routing_doc(&cp, id, older.clone())
        .await
        .expect("the older shape of a model that stores nothing");

    let settings = json!({
        "strategy": "consistent_hash",
        "retries": 0,
        "hash_on": [{"type": "header", "name": "x-tenant"}, {"type": "header", "name": "x-user"}],
    });
    let r = patch_public(&cp, &op, id, json!({"routing": settings})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let saved = live(&cp, Kind::Models, id).await;

    let refused = write_routing_doc(&cp, id, older.clone()).await;
    assert!(refused.is_err(), "the older shape dropped the settings");
    // A write keeping all but one setting, or one whose list is reordered,
    // drops it too.
    let mut without_zero = saved.clone();
    without_zero["routing"]
        .as_object_mut()
        .expect("routing")
        .remove("retries");
    assert!(write_routing_doc(&cp, id, without_zero).await.is_err());
    let mut reordered = saved.clone();
    reordered["routing"]["hash_on"] = json!([
        {"type": "header", "name": "x-user"},
        {"type": "header", "name": "x-tenant"},
    ]);
    assert!(write_routing_doc(&cp, id, reordered).await.is_err());
    assert_eq!(live(&cp, Kind::Models, id).await, saved);

    // This release's channel edits and resets still write.
    let r = put_channel(&cp, &op, key, id, json!({"priority": 4})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let doc = live(&cp, Kind::Models, id).await;
    assert_eq!(doc["routing"]["retries"], 0, "{doc}");
    assert_eq!(doc["routing"]["hash_on"], settings["hash_on"], "{doc}");
    let r = patch_public(&cp, &op, id, json!({"routing": {}})).await;
    assert_eq!(r.status, 200, "{r:?}");
    write_routing_doc(&cp, id, older)
        .await
        .expect("the settings are gone, so the older shape is today's again");
}

/// Whether the `kind`/`id` row's current version reaches kine within 20 s.
async fn until_projected(cp: &TestCp, kind: Kind, id: Uuid) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let done: bool = sqlx::query_scalar(
            "SELECT projected_version IS NOT DISTINCT FROM version FROM gw_resources \
             WHERE kind = $1 AND id = $2",
        )
        .bind(kind.segment())
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the row's projection");
        if done {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// A routing document a control plane older than `0037` wrote before
/// `0039`'s fence existed lacks the settings its public model stores. The
/// fence judges a write of the document, and nothing else: the projector's
/// own bookkeeping on the row (the projection columns it updates after each
/// kine write, which a reconcile also re-marks) must go through, or every
/// pass would stop at that row and no later document, a revocation included,
/// would reach the gateways. The housekeeping pass, which every boot runs
/// too, then regenerates the document from the stored settings, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_routing_document_that_diverged_before_the_fence_is_projected_and_then_regenerated() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let key = provider_key(&cp, &op, "acct-diverged-6a3").await;
    let (id, _) = create_public(&cp, &op, "pub-diverged-6a3").await;
    let r = put_channel(&cp, &op, key, id, json!({})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(until_projected(&cp, Kind::Models, id).await);
    let written = live(&cp, Kind::Models, id).await;

    // What the older writer left: settings stored beside a document without
    // them, whose projection is due again, as a reconcile that found kine
    // drifted marks it. Marked with the triggers off, so that only the
    // projector's own update meets the fence.
    let settings = json!({"strategy": "failover", "retries": 2});
    sqlx::query("UPDATE public_models SET routing = $2 WHERE id = $1")
        .bind(id)
        .bind(&settings)
        .execute(cp.db().pool())
        .await
        .expect("store the settings");
    let mut conn = <sqlx::PgConnection as sqlx::Connection>::connect(cp.database_url())
        .await
        .expect("connect");
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut conn)
        .await
        .expect("triggers off");
    sqlx::query("UPDATE gw_resources SET projected_version = NULL WHERE kind = $1 AND id = $2")
        .bind(Kind::Models.segment())
        .bind(id)
        .execute(&mut conn)
        .await
        .expect("mark the projection due");
    drop(conn);

    // The projector writes it again, and goes on to what was written after.
    let later = provider_key(&cp, &op, "acct-later-6a3").await;
    assert!(
        until_projected(&cp, Kind::ProviderKeys, later).await,
        "a later document never reached kine: the projector stopped at the diverged row"
    );
    assert!(until_projected(&cp, Kind::Models, id).await);
    assert_eq!(live(&cp, Kind::Models, id).await, written);

    // The housekeeping pass regenerates it from the stored settings.
    housekeeping::tick(cp.db(), &WriteCtx::of(cp.state())).await;
    let doc = live(&cp, Kind::Models, id).await;
    assert_eq!(doc["routing"]["strategy"], "failover", "{doc}");
    assert_eq!(doc["routing"]["retries"], 2, "{doc}");
    assert_eq!(
        doc["routing"]["targets"], written["routing"]["targets"],
        "{doc}"
    );
    assert_eq!(
        audit_detail(&cp, "public_model.routing_regenerated", id).await,
        json!({"name": "pub-diverged-6a3"})
    );
    assert!(until_projected(&cp, Kind::Models, id).await);
    // Once: nothing is left for the next pass.
    let again = channels::repair_routing(cp.db(), &WriteCtx::of(cp.state()))
        .await
        .expect("a pass");
    assert_eq!(again, channels::RoutingRepaired::default());
}
