//! A tenant's model access is an explicit scope: every public model, or the
//! list it holds.
//!
//! "All" is the live public models at the moment it is read, not a list
//! stored per tenant, so a public model created later is in every all-scope
//! tenant's key documents without an edit, and retiring one withdraws
//! nothing. A hand-made model is reachable only through the selected list.
//!
//! Every expected value comes from what the test itself writes; nothing here
//! is read off the product.

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::resources::Kind;
use futures::FutureExt;
use serde_json::{json, Value};
use support::migrate::{database_at, drop_database, migrate_rest};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// A tenant's live `api_keys` document.
async fn key_doc(cp: &TestCp, id: &Uuid) -> Value {
    let (doc, deleted): (Value, bool) =
        sqlx::query_as("SELECT doc, deleted FROM gw_resources WHERE kind = $1 AND id = $2")
            .bind(Kind::ApiKeys.segment())
            .bind(id)
            .fetch_one(cp.db().pool())
            .await
            .expect("the key document");
    assert!(!deleted, "api_keys/{id} is deleted");
    doc
}

/// The tenant as `GET /admin/tenants/{id}` answers it.
async fn tenant(cp: &TestCp, admin: &OperatorSession, id: Uuid) -> Resp {
    let r = cp
        .send(
            "GET",
            &format!("/admin/tenants/{id}"),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    r
}

/// `PATCH /admin/tenants/{id}` with the version it was just read at.
async fn patch(cp: &TestCp, admin: &OperatorSession, id: Uuid, body: Value) -> Resp {
    let current = tenant(cp, admin, id).await;
    let version = current.body["version"].as_i64().expect("a version");
    request(
        cp,
        "PATCH",
        &format!("/admin/tenants/{id}"),
        Some(admin.cookie()),
        Some(version),
        Some(body),
    )
    .await
}

/// A request as the Dashboard sends it: labelled JSON, `If-Match` when
/// given. `TestCp::send` carries no `If-Match`.
async fn request(
    cp: &TestCp,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    if_match: Option<i64>,
    body: Option<Value>,
) -> Resp {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
    let mut request = client
        .request(method, cp.url(path))
        .header(reqwest::header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        request = request.header(reqwest::header::COOKIE, cookie);
    }
    if let Some(v) = if_match {
        request = request.header(reqwest::header::IF_MATCH, format!("\"{v}\""));
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    Resp::read(request.send().await.expect("send the request")).await
}

/// A tenant created by an operator, active.
async fn operator_tenant(cp: &TestCp, admin: &OperatorSession, name: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/tenants",
            Some(admin.cookie()),
            Some(json!({"name": name})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("a tenant id")).expect("a uuid")
}

/// A public model, created as `admin`; its id.
async fn public_model(cp: &TestCp, admin: &OperatorSession, name: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/public-models",
            Some(admin.cookie()),
            Some(json!({"name": name})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("a public model id")).expect("a uuid")
}

/// An upstream account, created as `admin`; its id.
async fn provider_key(cp: &TestCp, admin: &OperatorSession, name: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": name,
                "provider": "openai",
                "api_key": "sk-scope-9e4",
                "groups": ["shared"],
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid")
}

/// A hand-made model on its own account, created as `admin`; its id.
async fn hand_made_model(cp: &TestCp, admin: &OperatorSession) -> Uuid {
    let account = provider_key(cp, admin, &format!("acct-{}", Uuid::new_v4().simple())).await;
    let r = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": format!("hand-{}", Uuid::new_v4().simple()),
                "provider": "openai",
                "model_name": "gpt-4o-mini",
                "provider_key_id": account,
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("a model id")).expect("a uuid")
}

/// Makes `public` callable: one enabled channel model on `admin`'s account,
/// which writes the routing document an all-scope tenant's scope is read
/// from.
async fn serve(cp: &TestCp, admin: &OperatorSession, account: Uuid, public: Uuid) {
    let r = cp
        .send(
            "PUT",
            &format!("/admin/provider-keys/{account}/channel-models/{public}"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// A key of `tenant`, minted by `admin`; its id.
async fn key(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, label: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(admin.cookie()),
            Some(json!({"label": label})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["key"]["id"].as_str().expect("a key id")).expect("a uuid")
}

/// The ids a key document names, sorted.
fn ids(doc: &Value) -> Vec<String> {
    let mut ids: Vec<String> = doc["allowed_model_ids"]
        .as_array()
        .expect("a written list")
        .iter()
        .map(|v| v.as_str().expect("a uuid").to_string())
        .collect();
    ids.sort();
    ids
}

fn uuid_list(ids: &[Uuid]) -> Vec<String> {
    let mut ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    ids.sort();
    ids
}

/// A new tenant of either origin starts on every public model, and a public
/// model created afterwards reaches its keys without an edit.
#[tokio::test]
async fn a_new_tenant_reaches_every_public_model_including_one_created_later() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &admin, "acct-scope-all-9e4").await;
    let first = public_model(&cp, &admin, "scope-pub-first-9e4").await;
    serve(&cp, &admin, account, first).await;

    // An operator's tenant: the default is every public model.
    let created = operator_tenant(&cp, &admin, "scope-operator-9e4").await;
    let view = tenant(&cp, &admin, created).await;
    assert_eq!(view.body["model_scope"], "all", "{view:?}");
    assert_eq!(view.body["allowed_model_ids"], json!([]), "{view:?}");

    // And so is a self-service sign-up's, before an operator approves it.
    let email = format!("scope-{}@tenant.test", Uuid::new_v4().simple());
    let r = cp
        .send(
            "POST",
            "/api/signup",
            None,
            Some(json!({
                "org_name": "acme-9e4",
                "email": email,
                "password": "scope-pass-9e4",
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let signed_up: Uuid =
        sqlx::query_scalar("SELECT id FROM tenants WHERE name = 'acme-9e4' AND status = 'pending'")
            .fetch_one(cp.db().pool())
            .await
            .expect("the signed-up tenant");
    let stored: String = sqlx::query_scalar("SELECT model_scope FROM tenants WHERE id = $1")
        .bind(signed_up)
        .fetch_one(cp.db().pool())
        .await
        .expect("its scope");
    assert_eq!(stored, "all");

    let key_id = key(&cp, &admin, created, "scope-all-9e4").await;
    assert_eq!(ids(&key_doc(&cp, &key_id).await), uuid_list(&[first]));

    // A public model created after the tenant: no edit to the tenant, and its
    // key still reaches both.
    let later = public_model(&cp, &admin, "scope-pub-later-9e4").await;
    serve(&cp, &admin, account, later).await;
    assert_eq!(
        ids(&key_doc(&cp, &key_id).await),
        uuid_list(&[first, later])
    );

    // A public model with no serving account has no document, so it is not
    // callable and not in scope.
    let unserved = public_model(&cp, &admin, "scope-pub-unserved-9e4").await;
    assert_eq!(
        ids(&key_doc(&cp, &key_id).await),
        uuid_list(&[first, later]),
        "{}",
        unserved
    );

    // A hand-made model is outside an all-scope tenant's reach.
    let hand = hand_made_model(&cp, &admin).await;
    assert!(!ids(&key_doc(&cp, &key_id).await).contains(&hand.to_string()));
}

/// The tenant-facing surfaces follow the scope: the model list and the price
/// list name every public model an all-scope tenant reaches, and a selected
/// one only what it lists.
#[tokio::test]
async fn the_tenant_surfaces_follow_the_scope() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &admin, "acct-scope-api-9e4").await;
    let public = public_model(&cp, &admin, "scope-pub-api-9e4").await;
    serve(&cp, &admin, account, public).await;
    let hand = hand_made_model(&cp, &admin).await;

    let created = operator_tenant(&cp, &admin, "scope-api-9e4").await;
    let owner = cp.member_login_seeded(created, MemberRole::Owner).await;

    let names = |body: &Value| -> Vec<String> {
        let mut names: Vec<String> = body["items"]
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|m| m["name"].as_str().map(str::to_string))
            .collect();
        names.sort();
        names
    };
    let r = cp
        .send("GET", "/api/models", Some(owner.cookie()), None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(names(&r.body), vec!["scope-pub-api-9e4".to_string()]);

    // Selected with a hand-made model: the public model leaves the list.
    let r = patch(
        &cp,
        &admin,
        created,
        json!({"model_scope": "selected", "allowed_model_ids": [hand]}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp
        .send("GET", "/api/models", Some(owner.cookie()), None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let hand_name: String = sqlx::query_scalar(
        "SELECT doc->>'display_name' FROM gw_resources WHERE kind = 'models' AND id = $1",
    )
    .bind(hand)
    .fetch_one(cp.db().pool())
    .await
    .expect("the hand-made model's name");
    assert_eq!(names(&r.body), vec![hand_name]);
}

/// Narrowing to `selected` without a list starts from what the tenant
/// reaches now, and removing its last model leaves it with none — never with
/// all.
#[tokio::test]
async fn narrowing_starts_from_the_current_set_and_an_empty_list_grants_none() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &admin, "acct-scope-narrow-9e4").await;
    let public = public_model(&cp, &admin, "scope-pub-narrow-9e4").await;
    serve(&cp, &admin, account, public).await;
    let created = operator_tenant(&cp, &admin, "scope-narrow-9e4").await;
    let key_id = key(&cp, &admin, created, "scope-narrow-key-9e4").await;
    assert_eq!(ids(&key_doc(&cp, &key_id).await), uuid_list(&[public]));

    // The switch alone snapshots the effective set rather than dropping it.
    let r = patch(&cp, &admin, created, json!({"model_scope": "selected"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["model_scope"], "selected", "{r:?}");
    assert_eq!(r.body["allowed_model_ids"], json!([public]), "{r:?}");
    assert_eq!(ids(&key_doc(&cp, &key_id).await), uuid_list(&[public]));

    // A public model created now does not reach it: selected is what it lists.
    let later = public_model(&cp, &admin, "scope-pub-narrow-later-9e4").await;
    serve(&cp, &admin, account, later).await;
    assert_eq!(ids(&key_doc(&cp, &key_id).await), uuid_list(&[public]));

    // The last removal leaves none, and the key keeps an empty list rather
    // than falling back to every model.
    let r = patch(&cp, &admin, created, json!({"allowed_model_ids": []})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let doc = key_doc(&cp, &key_id).await;
    assert_eq!(doc["allowed_model_ids"], json!([]), "{doc}");
    assert!(doc.get("allowed_models").is_none(), "{doc}");

    // Back to all: the stored list stands aside and the scope is the
    // catalogue again.
    let r = patch(&cp, &admin, created, json!({"model_scope": "all"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        ids(&key_doc(&cp, &key_id).await),
        uuid_list(&[public, later])
    );
}

/// A list is a selection: it is refused beside `all` and implies `selected`
/// on its own.
#[tokio::test]
async fn a_model_list_and_the_all_scope_are_mutually_exclusive() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let created = operator_tenant(&cp, &admin, "scope-check-9e4").await;
    let hand = hand_made_model(&cp, &admin).await;
    let r = patch(
        &cp,
        &admin,
        created,
        json!({"model_scope": "all", "allowed_model_ids": [hand]}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/allowed_model_ids", "{r:?}");
    // A list alone is a selection.
    let r = patch(&cp, &admin, created, json!({"allowed_model_ids": [hand]})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["model_scope"], "selected", "{r:?}");
    assert_eq!(r.body["allowed_model_ids"], json!([hand]), "{r:?}");
}

/// A key's own selection narrows within the tenant's scope, and an empty one
/// is the whole scope, as it always was.
#[tokio::test]
async fn a_keys_selection_narrows_within_the_scope() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &admin, "acct-scope-key-9e4").await;
    let first = public_model(&cp, &admin, "scope-pub-key-a-9e4").await;
    let second = public_model(&cp, &admin, "scope-pub-key-b-9e4").await;
    serve(&cp, &admin, account, first).await;
    serve(&cp, &admin, account, second).await;
    let created = operator_tenant(&cp, &admin, "scope-key-9e4").await;
    let hand = hand_made_model(&cp, &admin).await;

    // A selection of one public model, under the all scope.
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{created}/keys"),
            Some(admin.cookie()),
            Some(json!({"label": "scope-key-picked-9e4", "model_ids": [second]})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let picked = Uuid::parse_str(r.body["key"]["id"].as_str().expect("an id")).expect("a uuid");
    assert_eq!(ids(&key_doc(&cp, &picked).await), uuid_list(&[second]));

    // A model outside the scope is answered as one that does not exist.
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{created}/keys"),
            Some(admin.cookie()),
            Some(json!({"label": "scope-key-out-9e4", "model_ids": [hand]})),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/model_ids/0", "{r:?}");

    // A key with no selection is the whole scope: every callable public
    // model, and not the hand-made one.
    let all = key(&cp, &admin, created, "scope-key-all-9e4").await;
    assert_eq!(ids(&key_doc(&cp, &all).await), uuid_list(&[first, second]));
}

/// The tenant's price list follows the scope too: an all-scope tenant is
/// shown the price of every public model the catalogue can price, and a
/// selected one only of the models it lists.
#[tokio::test]
async fn the_price_list_follows_the_scope() {
    use aisix_control_plane::audit::Actor;
    use aisix_control_plane::pricing::rates::{self, NewGroupRate};
    use aisix_pricing::{CnyPerUsd, RateCard};
    use support::world::{db_now, insert_baseline};

    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &admin, "acct-scope-price-9e4").await;
    let public = public_model(&cp, &admin, "scope-pub-price-9e4").await;
    serve(&cp, &admin, account, public).await;
    // A baseline for the public model, and the shared group's rate for it:
    // the same price a hand-made model needs to be listed.
    let from: chrono::DateTime<chrono::Utc> = db_now(&cp).await;
    let card: RateCard =
        serde_json::from_value(json!({"default": {"base": {"input": "4", "output": "20"}}}))
            .expect("a card");
    insert_baseline(&cp, public, from, card)
        .await
        .expect("a baseline");
    let shared = support::world::group_id(&cp, "shared").await;
    let row = NewGroupRate {
        group_id: shared,
        model_id: Some(public),
        cny_per_usd: Some(CnyPerUsd::parse("1").expect("a rate")),
        effective_from: from,
        note: "the scope test's rate".to_string(),
    };
    cp.db()
        .tx(move |c| {
            async move {
                rates::insert_group_rate(c, &row, Actor::System)
                    .await
                    .map(|(revision, _)| revision)
            }
            .boxed()
        })
        .await
        .expect("the group rate");

    let created = operator_tenant(&cp, &admin, "scope-price-9e4").await;
    let owner = cp.member_login_seeded(created, MemberRole::Owner).await;
    let listed = |body: &Value| -> Vec<String> {
        body["groups"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|g| g["models"].as_array().into_iter().flatten())
            .filter_map(|m| m["name"].as_str().map(str::to_string))
            .collect()
    };
    let r = cp
        .send("GET", "/api/prices", Some(owner.cookie()), None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(listed(&r.body), vec!["scope-pub-price-9e4".to_string()]);

    // Selected with nothing: the tenant reaches no model, so no price.
    let r = patch(
        &cp,
        &admin,
        created,
        json!({"model_scope": "selected", "allowed_model_ids": []}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = cp
        .send("GET", "/api/prices", Some(owner.cookie()), None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(listed(&r.body).is_empty(), "{r:?}");
}

/// Retiring a public model is never refused because an all-scope tenant is
/// on it: the model simply stops being in that tenant's scope, and its key
/// documents stop naming it.
#[tokio::test]
async fn retiring_a_public_model_withdraws_it_from_an_all_scope_tenants_keys() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let account = provider_key(&cp, &admin, "acct-scope-retire-9e4").await;
    let kept = public_model(&cp, &admin, "scope-pub-kept-9e4").await;
    let gone = public_model(&cp, &admin, "scope-pub-gone-9e4").await;
    serve(&cp, &admin, account, kept).await;
    serve(&cp, &admin, account, gone).await;
    let created = operator_tenant(&cp, &admin, "scope-retire-9e4").await;
    let key_id = key(&cp, &admin, created, "scope-retire-key-9e4").await;
    assert_eq!(ids(&key_doc(&cp, &key_id).await), uuid_list(&[kept, gone]));
    // A key of the same tenant that names the model explicitly is still not
    // a reference: its selection narrows a scope that does not hold the
    // model, so the retire takes it away with the rest.
    let picking = cp
        .send(
            "POST",
            &format!("/admin/tenants/{created}/keys"),
            Some(admin.cookie()),
            Some(json!({"label": "scope-retire-picking-9e4", "model_ids": [gone]})),
        )
        .await;
    assert_eq!(picking.status, 201, "{picking:?}");
    let picking =
        Uuid::parse_str(picking.body["key"]["id"].as_str().expect("an id")).expect("a uuid");
    assert_eq!(ids(&key_doc(&cp, &picking).await), uuid_list(&[gone]));
    // The stored list is left naming the model through a round trip to
    // `all`: the count must read the scope, not the list, or a tenant that
    // no longer references the model would still refuse its retire. The key
    // here has no selection, so the key count has the same question.
    let r = patch(&cp, &admin, created, json!({"allowed_model_ids": [gone]})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = patch(&cp, &admin, created, json!({"model_scope": "all"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["allowed_model_ids"], json!([gone]), "{r:?}");

    // While the account still serves it the retire is refused, and the
    // counts name the serving row alone: this tenant, on `all` with a stale
    // list naming the model, and its key with no selection, are not
    // references.
    let r = cp
        .send(
            "POST",
            &format!("/admin/public-models/{gone}/retire"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "public_model_in_use", "{r:?}");
    assert_eq!(
        r.body["counts"],
        json!({"tenants": 0, "keys": 0, "channels": 1}),
        "{r:?}"
    );
    // Then it serves nothing: the account that did is an ordinary channel
    // delete.
    let r = cp
        .send(
            "DELETE",
            &format!("/admin/provider-keys/{account}/channel-models/{gone}"),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    let r = cp
        .send(
            "POST",
            &format!("/admin/public-models/{gone}/retire"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(ids(&key_doc(&cp, &key_id).await), uuid_list(&[kept]));
    // The selected key keeps the list it holds, now empty: its tenant no
    // longer reaches the model, and an absent list would fall back to names.
    let doc = key_doc(&cp, &picking).await;
    assert_eq!(doc["allowed_model_ids"], json!([]), "{doc}");
    assert!(doc.get("allowed_models").is_none(), "{doc}");
}

/// The upgrade: a tenant that already lists models becomes `selected` with
/// exactly those, and one that lists none becomes `all`. A row written
/// without a scope in the column loads as `selected` — never as all.
#[tokio::test]
async fn the_upgrade_reads_the_stored_grants() {
    let Some(db) = database_at(30).await else {
        return;
    };
    let listed = Uuid::new_v4();
    let bare = Uuid::new_v4();
    let empty_too = Uuid::new_v4();
    let internal = Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO tenants (id, name, status, allowed_model_ids) VALUES \
             ('{listed}', 'listed-9e4', 'active', '{{{}}}')",
            listed.simple()
        ),
        format!("INSERT INTO tenants (id, name, status) VALUES ('{bare}', 'bare-9e4', 'active')"),
        format!(
            "INSERT INTO tenants (id, name, status) VALUES \
             ('{empty_too}', 'empty-too-9e4', 'active')"
        ),
        format!(
            "INSERT INTO tenants (id, name, status, is_internal) VALUES \
             ('{internal}', 'platform-9e4', 'active', true)"
        ),
    ] {
        sqlx::query(&sql).execute(&db).await.expect(&sql);
    }

    migrate_rest(&db).await.expect("the rest migrates");

    let scope = |id: Uuid| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (String, Vec<Uuid>)>(
                "SELECT model_scope, allowed_model_ids FROM tenants WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .expect("the migrated row")
        }
    };
    // A tenant that lists a model keeps exactly that, and reaches nothing
    // more.
    assert_eq!(scope(listed).await, ("selected".to_string(), vec![listed]));
    // One that lists none reaches every public model.
    assert_eq!(scope(bare).await, ("all".to_string(), Vec::new()));
    assert_eq!(scope(empty_too).await, ("all".to_string(), Vec::new()));
    // The platform's own tenant stays on its list, which reaches nothing
    // beyond the models an operator grants it by id.
    assert_eq!(scope(internal).await, ("selected".to_string(), Vec::new()));

    // The widening is auditable, and only the tenant it happened to is in
    // the record: the one that listed nothing and is not the platform's.
    let widened: Vec<(String, Value, Option<Uuid>)> = sqlx::query_as(
        "SELECT target_id, detail, tenant_id FROM audit_log \
         WHERE action = 'tenant.model_scope_widened' ORDER BY target_id",
    )
    .fetch_all(&db)
    .await
    .expect("the widening's audit rows");
    // Exactly the tenants widened, by name and id: the two that listed
    // nothing and are not the platform's, and neither of the others.
    let mut recorded: Vec<(&str, Uuid)> = widened
        .iter()
        .map(|(target, detail, tenant_id)| {
            let id = tenant_id.expect("a tenant id");
            assert_eq!(*target, id.to_string(), "{detail}");
            assert_eq!(detail["from"], "selected", "{detail}");
            assert_eq!(detail["to"], "all", "{detail}");
            (detail["name"].as_str().expect("a name"), id)
        })
        .collect();
    let mut expected = vec![("bare-9e4", bare), ("empty-too-9e4", empty_too)];
    recorded.sort_by_key(|(name, _)| *name);
    expected.sort_by_key(|(name, _)| *name);
    assert_eq!(recorded, expected, "{widened:?}");

    // A row written without the column — an older replica during a rolling
    // upgrade, or a restore that omits it — takes the fail-closed default:
    // selected, with the list it carries.
    let mut conn = db.acquire().await.expect("a connection");
    let (name, ids) = ("written-9e4".to_string(), vec![Uuid::new_v4()]);
    let (scope, stored): (String, Vec<Uuid>) = sqlx::query_as(
        "INSERT INTO tenants (id, name, status, allowed_model_ids) \
         VALUES ($1, $2, 'active', $3) RETURNING model_scope, allowed_model_ids",
    )
    .bind(Uuid::new_v4())
    .bind(&name)
    .bind(&ids)
    .fetch_one(&mut *conn)
    .await
    .expect("insert a row without a scope");
    assert_eq!(scope, "selected");
    assert_eq!(stored, ids);
    drop(conn);

    drop_database(db).await;
}
