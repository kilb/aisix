//! A priced world for the pricing and billing tests: an operator, a gateway,
//! a tenant with one member and that member's key, models on provider keys
//! of their own, platform prices, and usage rows delivered over
//! `POST /dp/telemetry` exactly as a gateway sends them.

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::db::Db;
use aisix_control_plane::error::ApiError;
use aisix_control_plane::pricing::store::{self, NewSellPrice, SellRevision};
use aisix_pricing::{RateCard, ENGINE_VERSION};
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{DpClient, OperatorSession, Resp, TestCp};

/// USD per million: input 1, output 2, cache read 0.1. So 1000 prompt tokens
/// of which 800 were cache hits, and 200 completion tokens, cost
/// 200 × 1 + 800 × 0.1 + 200 × 2 = 680 µ$.
pub fn card() -> RateCard {
    serde_json::from_value(json!({
        "default": {"base": {"input": "1", "output": "2", "cache_read": "0.1"}}
    }))
    .expect("the test card decodes")
}

/// The database's clock: what a price write compares `effective_from` with.
pub async fn db_now(cp: &TestCp) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT now()")
        .fetch_one(cp.db().pool())
        .await
        .expect("read now()")
}

fn text(v: &Value, pointer: &str) -> String {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{pointer} in {v}"))
        .to_string()
}

/// `POST path` as `admin`, which must answer 201.
async fn create(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let r = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(r.status, 201, "POST {path}: {r:?}");
    r.body
}

/// An upstream account (a provider key), created as an operator; its id.
pub async fn seed_provider_key(cp: &TestCp, admin: &OperatorSession) -> Uuid {
    let pk = create(
        cp,
        admin,
        "/admin/resources/provider_keys",
        json!({"doc": {
            "display_name": format!("acct-pricing-{}", Uuid::new_v4().simple()),
            "provider": "openai",
            "api_key": "sk-pricing-upstream-7c1",
            "groups": ["shared"],
        }}),
    )
    .await;
    Uuid::parse_str(&text(&pk, "/id")).expect("a provider key id")
}

/// A platform model on a provider key of its own, created as an operator.
pub async fn seed_model(cp: &TestCp, admin: &OperatorSession) -> Uuid {
    let pk = seed_provider_key(cp, admin).await;
    let model = create(
        cp,
        admin,
        "/admin/resources/models",
        json!({"doc": {
            "display_name": format!("priced-{}", Uuid::new_v4().simple()),
            "provider": "openai",
            "model_name": "gpt-4o-mini",
            "provider_key_id": pk,
        }}),
    )
    .await;
    Uuid::parse_str(&text(&model, "/id")).expect("a model id")
}

pub async fn seed_tenant(cp: &TestCp, admin: &OperatorSession) -> Uuid {
    let tenant = create(
        cp,
        admin,
        "/admin/tenants",
        json!({"name": format!("tenant-{}", Uuid::new_v4().simple())}),
    )
    .await;
    Uuid::parse_str(&text(&tenant, "/id")).expect("a tenant id")
}

/// The group a seeded tenant is assigned by the first-boot defaults
/// (`settings::TenantDefaults::first_boot`), which every seeded tenant and
/// its keys therefore use.
pub const SHARED_GROUP: &str = "shared";

/// The id of the live group `name`, which a migration or a `POST
/// /admin/provider-groups` created.
pub async fn group_id(cp: &TestCp, name: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = $1 AND retired_at IS NULL")
        .bind(name)
        .fetch_one(cp.db().pool())
        .await
        .unwrap_or_else(|e| panic!("group {name}: {e}"))
}

/// A key of `tenant`, bound to `member` when given, minted by an operator
/// and naming group `group`; its id.
pub async fn seed_key_in_group(
    cp: &TestCp,
    admin: &OperatorSession,
    tenant: Uuid,
    member: Option<Uuid>,
    group: &str,
) -> String {
    let mut body = json!({"label": "pricing-7c1", "group_id": group_id(cp, group).await});
    if let Some(member) = member {
        body["member_id"] = json!(member);
    }
    let key = create(cp, admin, &format!("/admin/tenants/{tenant}/keys"), body).await;
    text(&key, "/key/id")
}

/// A key of `tenant`, bound to `member` when given, minted by an operator
/// and naming the group a seeded tenant is assigned; its id.
pub async fn seed_key(
    cp: &TestCp,
    admin: &OperatorSession,
    tenant: Uuid,
    member: Option<Uuid>,
) -> String {
    seed_key_in_group(cp, admin, tenant, member, SHARED_GROUP).await
}

/// A control plane with an operator, a gateway, a tenant, one of its members
/// and that member's key.
pub struct World {
    pub cp: TestCp,
    pub admin: OperatorSession,
    pub dp: DpClient,
    pub tenant: Uuid,
    pub member: Uuid,
    pub key: String,
}

pub async fn world() -> Option<World> {
    let cp = TestCp::start().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-pricing-7c1").await;
    let tenant = seed_tenant(&cp, &admin).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let key = seed_key(&cp, &admin, tenant, Some(member)).await;
    Some(World {
        cp,
        admin,
        dp,
        tenant,
        member,
        key,
    })
}

pub async fn insert_sell(
    cp: &TestCp,
    model_id: Uuid,
    tenant_id: Option<Uuid>,
    effective_from: DateTime<Utc>,
    card: Option<RateCard>,
) -> Result<SellRevision, ApiError> {
    let row = NewSellPrice {
        model_id,
        tenant_id,
        effective_from,
        card,
        source: "manual".to_string(),
    };
    cp.db()
        .tx(move |c| async move { store::insert_sell_price(c, &row, Actor::System).await }.boxed())
        .await
}

/// The platform price `card()` for `model`, starting now.
pub async fn price(cp: &TestCp, model: Uuid) -> SellRevision {
    let now = db_now(cp).await;
    insert_sell(cp, model, None, now, Some(card()))
        .await
        .expect("price the model")
}

/// A platform sell revision of `card` for `model_id` starting at
/// `effective_from`, past or not, written by raw SQL with the
/// `pricing_version` bump a price write makes, in one transaction: for rows
/// dated in the past, which `insert_sell_price` cannot price (it refuses a
/// backdate).
pub async fn insert_sell_backdated(
    db: &Db,
    model_id: Uuid,
    card: Value,
    effective_from: DateTime<Utc>,
) {
    let mut tx = db.pool().begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO pricing_sell_revisions (engine_version, model_id, effective_from, card, source) \
         VALUES ($1, $2, $3, $4, 'manual')",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(model_id)
    .bind(effective_from)
    .bind(card)
    .execute(&mut *tx)
    .await
    .expect("write a backdated revision");
    sqlx::query(
        "UPDATE cp_settings SET value = to_jsonb((value::text)::bigint + 1), updated_at = now() \
         WHERE key = 'pricing_version'",
    )
    .execute(&mut *tx)
    .await
    .expect("bump pricing_version");
    tx.commit().await.expect("commit");
}

/// A chat usage row of the world's tenant, member and key for `model`,
/// occurring now, with `fields` on top. It claims a `cost_usd` no price would
/// give, which pricing must never read.
pub fn usage(w: &World, event_id: &str, model: Uuid, fields: Value) -> Value {
    let mut event = json!({
        "event_id": event_id,
        "request_id": format!("req-{event_id}"),
        "occurred_at": Utc::now().to_rfc3339(),
        "team_id": w.tenant.to_string(),
        "api_key_id": w.key,
        "user_id": w.member.to_string(),
        "requested_model_id": model.to_string(),
        "operation": "chat",
        "status_code": 200,
        "prompt_tokens": 0,
        "completion_tokens": 0,
        "cost_usd": 12.5,
    });
    for (k, v) in fields.as_object().expect("fields are an object") {
        event[k] = v.clone();
    }
    event
}

/// 1000 prompt tokens, 800 of them cache hits, and 200 completion tokens:
/// 680 µ$ at `card()`.
pub fn tokens_680() -> Value {
    json!({"prompt_tokens": 1000, "cached_prompt_tokens": 800, "completion_tokens": 200})
}

pub async fn deliver(dp: &DpClient, batch_id: Uuid, events: Vec<Value>) -> Resp {
    dp.telemetry(batch_id, &json!({ "events": events })).await
}

/// `count` usage rows of key `key` of `tenant` for `model`, written by raw
/// SQL with an `unpriced` (`missing_price`) charge each, as intake stores a
/// row on a model without a price: event ids `<prefix>-1` to
/// `<prefix>-<count>`, occurring `from` plus 1 ms, 2 ms and so on, each of
/// 1000 prompt tokens (800 cache hits) and 200 completion tokens, which is
/// 680 µ$ at `card()`. For volumes the gateway path would take minutes to
/// deliver, and for tests with no gateway.
pub async fn insert_unpriced_charges(
    db: &Db,
    prefix: &str,
    count: i64,
    tenant: Uuid,
    key: Uuid,
    model: Uuid,
    from: DateTime<Utc>,
) {
    let mut tx = db.pool().begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, event) \
         SELECT $1 || '-' || n, $2, $2, $3 + n * interval '1 millisecond', $4::text, $5::text, \
                jsonb_build_object('event_id', $1 || '-' || n, \
                  'occurred_at', to_char(($3 + n * interval '1 millisecond') AT TIME ZONE 'UTC', \
                                         'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
                  'team_id', $4::text, 'api_key_id', $5::text, 'user_id', '', \
                  'requested_model_id', $6::text, 'operation', 'chat', 'status_code', 200, \
                  'is_final_attempt', true, 'prompt_tokens', 1000, 'cached_prompt_tokens', 800, \
                  'completion_tokens', 200) \
         FROM generate_series(1, $7::bigint) AS n",
    )
    .bind(prefix)
    .bind(Uuid::new_v4())
    .bind(from)
    .bind(tenant)
    .bind(key)
    .bind(model)
    .bind(count)
    .execute(&mut *tx)
    .await
    .expect("insert the usage rows");
    sqlx::query(
        "INSERT INTO pricing_charges \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, status, \
          error_code, engine_version) \
         SELECT event_id, team_id, api_key_id, '', $3::text, occurred_at, 'unpriced', \
                'missing_price', $4 \
         FROM usage_events WHERE event_id LIKE $1 || '-%' AND team_id = $2::text",
    )
    .bind(prefix)
    .bind(tenant)
    .bind(model)
    .bind(i64::from(ENGINE_VERSION))
    .execute(&mut *tx)
    .await
    .expect("insert their charges");
    tx.commit().await.expect("commit");
}

/// A public model named `name`, created as `admin`; its id.
///
/// A public model on its own writes no document: tick it on an upstream
/// account through `PUT /admin/provider-keys/{key}/channel-models/{id}` for
/// the routing document to exist.
pub async fn public_model(cp: &TestCp, admin: &OperatorSession, name: &str) -> Uuid {
    let body = create(cp, admin, "/admin/public-models", json!({"name": name})).await;
    Uuid::parse_str(&text(&body, "/id")).expect("a public model id")
}
