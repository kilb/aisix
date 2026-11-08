//! What one request id cost the caller (`GET /dp/v1/keys/{key_id}/requests`):
//! a gateway asks on behalf of a key, and the answer lists every row the
//! key's own tenant recorded under that id that the key may see, newest
//! first, with each row's corrected cost and the sum of those costs. A
//! request id is the caller's own input, so an id another tenant used
//! answers exactly like one never sent.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it.

mod support;

use std::collections::BTreeSet;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_pricing::RateCard;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use support::world::{
    db_now, deliver, insert_sell, insert_sell_backdated, price, seed_key, seed_model, seed_tenant,
    tokens_680, usage, world, World,
};
use support::{DpClient, Resp, TestCp};
use uuid::Uuid;

/// Task 12's channel fixture tokens: none may reach a lookup.
const PROVIDER_KEY: &str = "acct-bill-7c1";
const GROUP: &str = "grp-bill-5d2";
const UPSTREAM_MODEL: &str = "upstream-mdl-9f3";
const UPSTREAM_ERROR: &str = "upstream secret 4b7";
/// The `cost_usd` a row claims: neither the field nor its value may reach
/// an answer.
const CLAIMED_COST: f64 = 98.7654;

/// The keys of an answer and of one of its items, from the API section.
const ANSWER_KEYS: [&str; 5] = ["request_id", "total_usd", "pending", "has_more", "items"];
const ITEM_KEYS: [&str; 8] = [
    "occurred_at",
    "model",
    "status",
    "error",
    "charge_status",
    "charge_usd",
    "estimated",
    "quantities",
];

/// `GROUP`, the second group the fixture's account lists. A provider key
/// names only live groups, so it is created first, once per test's control
/// plane: a second account in the same test joins the group it made.
async fn group_once(w: &World) {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM provider_groups WHERE name = $1)")
            .bind(GROUP)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read the group");
    if !exists {
        support::seed_group(&w.cp, &w.admin, GROUP).await;
    }
}

/// A model named `name` on a provider key and groups that carry the channel
/// fixture tokens, priced at `card()` since a day ago when `priced`, and
/// granted to the world's tenant.
async fn model(w: &World, name: &str, priced: bool) -> Uuid {
    group_once(w).await;
    let pk =
        w.cp.send(
            "POST",
            "/admin/resources/provider_keys",
            Some(w.admin.cookie()),
            Some(json!({"doc": {
                "display_name": format!("{PROVIDER_KEY}-{name}"),
                "provider": "openai",
                "api_key": "sk-lookup-upstream-7c1",
                "groups": ["shared", GROUP],
            }})),
        )
        .await;
    assert_eq!(pk.status, 201, "{pk:?}");
    let m =
        w.cp.send(
            "POST",
            "/admin/resources/models",
            Some(w.admin.cookie()),
            Some(json!({"doc": {
                "display_name": name,
                "provider": "openai",
                "model_name": UPSTREAM_MODEL,
                "provider_key_id": pk.body["id"],
            }})),
        )
        .await;
    assert_eq!(m.status, 201, "{m:?}");
    let id = Uuid::parse_str(m.body["id"].as_str().expect("an id")).expect("a uuid");
    if priced {
        let since = db_now(&w.cp).await - TimeDelta::days(1);
        insert_sell_backdated(
            w.cp.db(),
            id,
            json!({"default": {"base": {"input": "1", "output": "2", "cache_read": "0.1"}}}),
            since,
        )
        .await;
    }
    sqlx::query(
        "UPDATE tenants SET allowed_model_ids = array_append(allowed_model_ids, $2) WHERE id = $1",
    )
    .bind(w.tenant)
    .bind(id)
    .execute(w.cp.db().pool())
    .await
    .expect("grant the model");
    id
}

/// A usage row of the world's key under `request_id`, occurring at `at`, as
/// the gateway records an attempt (final unless `fields` says otherwise),
/// carrying the channel fields a caller must never see.
fn row(
    w: &World,
    event_id: &str,
    request_id: &str,
    model: Uuid,
    at: DateTime<Utc>,
    fields: Value,
) -> Value {
    let mut event = usage(
        w,
        event_id,
        model,
        json!({
            "request_id": request_id,
            "occurred_at": at.to_rfc3339(),
            "is_final_attempt": true,
            "requested_model": "chat-bill",
            "model_id": model.to_string(),
            "attempt_model": UPSTREAM_MODEL,
            "provider_key_name": PROVIDER_KEY,
            "provider_group": GROUP,
            "cost_usd": CLAIMED_COST,
        }),
    );
    for (k, v) in fields.as_object().expect("fields are an object") {
        event[k] = v.clone();
    }
    event
}

/// `row` on another key of the world's tenant, bound to `member`.
fn row_of(
    w: &World,
    key: &str,
    member: Option<Uuid>,
    event_id: &str,
    request_id: &str,
    model: Uuid,
    at: DateTime<Utc>,
) -> Value {
    let mut event = row(w, event_id, request_id, model, at, tokens_680());
    event["api_key_id"] = json!(key);
    event["user_id"] = json!(member.map(|m| m.to_string()).unwrap_or_default());
    event
}

/// 4000 prompt and 3000 completion tokens: 10 000 µ$ at `card()`.
fn tokens_10000() -> Value {
    json!({"prompt_tokens": 4000, "completion_tokens": 3000})
}

/// A failed upstream attempt with its own status and text.
fn upstream_failure(fields: Value) -> Value {
    let mut f = json!({
        "status_code": 529,
        "error_class": "upstream_status",
        "error_message": UPSTREAM_ERROR,
    });
    for (k, v) in fields.as_object().expect("an object") {
        f[k] = v.clone();
    }
    f
}

async fn send_all(w: &World, events: Vec<Value>) {
    let r = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["rejected"], json!(0), "{r:?}");
}

/// A minute before the database's now, in whole seconds: rows occur from
/// there on, in the past and under the models' prices, and a timestamp
/// round-trips exactly and prints no fraction.
async fn base(cp: &TestCp) -> DateTime<Utc> {
    let now = db_now(cp).await;
    DateTime::from_timestamp(now.timestamp() - 60, 0).expect("a whole second")
}

/// `GET /dp/v1/keys/{key}/requests?id=<id>`, `id` as sent on the wire.
async fn lookup_raw(dp: &DpClient, key: &str, encoded_id: &str) -> Resp {
    dp.send(
        "GET",
        &format!("/dp/v1/keys/{key}/requests?id={encoded_id}"),
        None,
    )
    .await
}

/// `lookup_raw` with `id` percent-encoded.
async fn lookup(dp: &DpClient, key: &str, id: &str) -> Resp {
    let encoded: String = id
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    lookup_raw(dp, key, &encoded).await
}

fn items(r: &Resp) -> &Vec<Value> {
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items")
}

fn keys_of(v: &Value) -> BTreeSet<String> {
    v.as_object().expect("an object").keys().cloned().collect()
}

fn set(of: &[&str]) -> BTreeSet<String> {
    of.iter().map(|s| s.to_string()).collect()
}

fn time(v: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(v.as_str().expect("a timestamp"))
        .expect("RFC 3339")
        .with_timezone(&Utc)
}

/// A new platform price of `input` USD per million input tokens for `model`,
/// output and cache reads as `card()`, starting now.
async fn reprice_input(cp: &TestCp, model: Uuid, input: &str) {
    let card: RateCard = serde_json::from_value(json!({
        "default": {"base": {"input": input, "output": "2", "cache_read": "0.1"}}
    }))
    .expect("a card");
    let now = db_now(cp).await;
    insert_sell(cp, model, None, now, Some(card))
        .await
        .expect("a new price");
}

/// Previews and applies, as an admin, a re-pricing run over the last hour.
async fn run_repricing(cp: &TestCp) {
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let to = db_now(cp).await;
    let r = cp
        .send(
            "POST",
            "/admin/billing/repricing",
            Some(admin.cookie()),
            Some(json!({
                "from": to - TimeDelta::hours(1),
                "to": to,
                "reason": "补录漏配的价格 7c1",
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let id = r.body["id"].as_str().expect("a run id").to_string();
    let r = cp
        .send(
            "POST",
            &format!("/admin/billing/repricing/{id}/apply"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

#[tokio::test]
async fn a_request_lookup_lists_the_callers_rows_newest_first() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let t0 = base(&w.cp).await;
    // A failover attempt the upstream did not bill, one it billed, then the
    // request's last attempt a second later. The event ids of the last two
    // sort the other way round, so the order of arrival does not stand in
    // for the order of occurrence.
    send_all(
        &w,
        vec![
            row(
                &w,
                "lk1-c",
                "req-lk-7c1",
                m,
                t0 - TimeDelta::seconds(1),
                upstream_failure(json!({"is_final_attempt": false})),
            ),
            row(
                &w,
                "lk1-b",
                "req-lk-7c1",
                m,
                t0,
                upstream_failure(json!({
                    "is_final_attempt": false,
                    "prompt_tokens": 1000,
                    "cached_prompt_tokens": 800,
                    "completion_tokens": 200,
                })),
            ),
            row(
                &w,
                "lk1-a",
                "req-lk-7c1",
                m,
                t0 + TimeDelta::seconds(1),
                tokens_10000(),
            ),
        ],
    )
    .await;
    let r = lookup(&w.dp, &w.key, "req-lk-7c1").await;
    assert_eq!(keys_of(&r.body), set(&ANSWER_KEYS), "{r:?}");
    assert_eq!(r.body["request_id"], "req-lk-7c1");
    assert_eq!(r.body["total_usd"], "0.010680");
    assert_eq!(r.body["pending"], false);
    assert_eq!(r.body["has_more"], false);
    let items = items(&r);
    let statuses: Vec<&Value> = items.iter().map(|i| &i["status"]).collect();
    assert_eq!(statuses, [&json!("succeeded"), &json!("retried")], "{r:?}");
    for item in items {
        assert_eq!(keys_of(item), set(&ITEM_KEYS), "{item}");
        assert_eq!(item["model"], "chat-bill", "{item}");
        assert_eq!(item["error"], Value::Null, "{item}");
        assert_eq!(item["charge_status"], "charged", "{item}");
        assert_eq!(item["estimated"], false, "{item}");
    }
    assert_eq!(time(&items[0]["occurred_at"]), t0 + TimeDelta::seconds(1));
    assert_eq!(time(&items[1]["occurred_at"]), t0);
    assert_eq!(items[0]["charge_usd"], "0.010000");
    assert_eq!(items[1]["charge_usd"], "0.000680");
    assert_eq!(
        items[0]["quantities"],
        json!({"input": 4000, "output": 3000})
    );
    assert_eq!(
        items[1]["quantities"],
        json!({"input": 200, "input_cache_read": 800, "output": 200})
    );

    // Matched exactly: neither a prefix, a pattern nor another case finds it.
    for near in [
        "req-lk-7c",
        "req-lk-%",
        "req-lk-7c_",
        "REQ-LK-7C1",
        "req-lk-7c1x",
    ] {
        assert_eq!(lookup(&w.dp, &w.key, near).await.status, 404, "{near}");
    }
}

#[tokio::test]
async fn a_reused_request_id_lists_every_row_newest_first() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let t0 = base(&w.cp).await;
    // 60 requests under one id, one a millisecond after another, with event
    // ids sorting the other way round; and, after them, 5 failover attempts
    // the upstream did not bill, which are not listed and take no place.
    let mut events: Vec<Value> = (0..60)
        .map(|n| {
            row(
                &w,
                &format!("dup-{:02}", 59 - n),
                "req-lk-dup",
                m,
                t0 + TimeDelta::milliseconds(n),
                tokens_680(),
            )
        })
        .collect();
    events.extend((0..5).map(|n| {
        row(
            &w,
            &format!("dup-hidden-{n}"),
            "req-lk-dup",
            m,
            t0 + TimeDelta::milliseconds(100 + n),
            upstream_failure(json!({"is_final_attempt": false})),
        )
    }));
    send_all(&w, events).await;
    let r = lookup(&w.dp, &w.key, "req-lk-dup").await;
    let items = items(&r);
    assert_eq!(items.len(), 50, "{r:?}");
    assert_eq!(r.body["has_more"], true);
    let shown: Vec<DateTime<Utc>> = items.iter().map(|i| time(&i["occurred_at"])).collect();
    let newest: Vec<DateTime<Utc>> = (10..60)
        .rev()
        .map(|n| t0 + TimeDelta::milliseconds(n))
        .collect();
    assert_eq!(shown, newest);
    // The total is the listed rows' sum: 50 × 680 µ$.
    assert_eq!(r.body["total_usd"], "0.034000");
    assert_eq!(r.body["pending"], false);
}

#[tokio::test]
async fn another_tenants_request_id_answers_like_an_unknown_one() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let a = w.tenant;
    let b = seed_tenant(&w.cp, &w.admin).await;
    let b_key = seed_key(&w.cp, &w.admin, b, None).await;
    let t0 = base(&w.cp).await;
    // B's own row, and a row B's key sent claiming A's tenant: both are
    // filed under B, the tenant of the key.
    let mut theirs = row(&w, "b-own", "req-lk-9f3", m, t0, tokens_680());
    theirs["team_id"] = json!(b.to_string());
    theirs["api_key_id"] = json!(b_key);
    theirs["user_id"] = json!("");
    let mut forged = row(&w, "b-forged", "req-lk-9f3", m, t0, tokens_680());
    forged["team_id"] = json!(a.to_string());
    forged["api_key_id"] = json!(b_key);
    forged["user_id"] = json!("");
    send_all(&w, vec![theirs, forged]).await;

    // B sees its rows under the id.
    assert_eq!(items(&lookup(&w.dp, &b_key, "req-lk-9f3").await).len(), 2);

    // A's keys, its owner's (which sees every row of A) and its member's,
    // get what an id never sent gets, to the byte.
    let owner = w.cp.seed_member(a, MemberRole::Owner).await.id;
    let owner_key = seed_key(&w.cp, &w.admin, a, Some(owner)).await;
    let never = lookup(&w.dp, &owner_key, "req-lk-never-3a1").await;
    assert_eq!(never.status, 404, "{never:?}");
    for key in [&owner_key, &w.key] {
        let used = lookup(&w.dp, key, "req-lk-9f3").await;
        assert_eq!((used.status, &used.body), (never.status, &never.body));
        for header in ["content-type", "content-length"] {
            assert_eq!(
                used.headers.get(header),
                never.headers.get(header),
                "{header}"
            );
        }
    }
    // And so does a key that does not exist, and B's key once revoked.
    let unknown = lookup(&w.dp, &Uuid::new_v4().to_string(), "req-lk-9f3").await;
    assert_eq!((unknown.status, &unknown.body), (never.status, &never.body));
    revoke(&w, &b_key).await;
    let revoked = lookup(&w.dp, &b_key, "req-lk-9f3").await;
    assert_eq!((revoked.status, &revoked.body), (never.status, &never.body));
}

/// Revokes key `key` as the world's operator.
async fn revoke(w: &World, key: &str) {
    let key = Uuid::parse_str(key).expect("a key id");
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id = $1")
        .bind(key)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("read the key");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a client")
        .delete(w.cp.url(&format!("/admin/keys/{key}")))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, w.admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .send()
        .await
        .expect("send");
    assert!(r.status().is_success(), "{r:?}");
}

#[tokio::test]
async fn a_member_key_sees_only_its_members_rows_for_a_request_id() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let owner = w.cp.seed_member(w.tenant, MemberRole::Owner).await.id;
    let owner_key = seed_key(&w.cp, &w.admin, w.tenant, Some(owner)).await;
    let t0 = base(&w.cp).await;
    // Under one id, a row of K (the world's member's key) and one of the
    // owner's key.
    send_all(
        &w,
        vec![
            row(&w, "mk-k", "req-lk-2e8", m, t0, tokens_680()),
            row_of(&w, &owner_key, Some(owner), "mk-o", "req-lk-2e8", m, t0),
        ],
    )
    .await;
    assert_eq!(items(&lookup(&w.dp, &w.key, "req-lk-2e8").await).len(), 1);
    assert_eq!(
        items(&lookup(&w.dp, &owner_key, "req-lk-2e8").await).len(),
        2
    );
    // An admin's key sees the tenant's rows too.
    let admin = w.cp.seed_member(w.tenant, MemberRole::Admin).await.id;
    let admin_key = seed_key(&w.cp, &w.admin, w.tenant, Some(admin)).await;
    assert_eq!(
        items(&lookup(&w.dp, &admin_key, "req-lk-2e8").await).len(),
        2
    );

    // A member's other key sees the member's rows; a key bound to no member
    // sees its own rows only.
    let second = seed_key(&w.cp, &w.admin, w.tenant, Some(w.member)).await;
    let unbound = seed_key(&w.cp, &w.admin, w.tenant, None).await;
    send_all(
        &w,
        vec![
            row(&w, "mk2-k", "req-lk-6e4", m, t0, tokens_680()),
            row_of(&w, &unbound, None, "mk2-u", "req-lk-6e4", m, t0),
        ],
    )
    .await;
    let r = lookup(&w.dp, &second, "req-lk-6e4").await;
    assert_eq!(items(&r).len(), 1, "{r:?}");
    let r = lookup(&w.dp, &unbound, "req-lk-6e4").await;
    assert_eq!(items(&r).len(), 1, "{r:?}");
    assert_eq!(
        items(&lookup(&w.dp, &owner_key, "req-lk-6e4").await).len(),
        2
    );
    // The member's key does not see the unbound key's row of the id it
    // shares with nobody else.
    let only_unbound = row_of(&w, &unbound, None, "mk3-u", "req-lk-0c5", m, t0);
    send_all(&w, vec![only_unbound]).await;
    assert_eq!(lookup(&w.dp, &w.key, "req-lk-0c5").await.status, 404);
}

#[tokio::test]
async fn an_unpriced_row_makes_the_total_pending() {
    let Some(w) = world().await else { return };
    let priced = model(&w, "chat-bill", true).await;
    let unpriced = seed_model(&w.cp, &w.admin).await;
    let t0 = base(&w.cp).await;
    send_all(
        &w,
        vec![
            row(
                &w,
                "up-1",
                "req-lk-4b7",
                priced,
                t0,
                upstream_failure(json!({
                    "is_final_attempt": false,
                    "prompt_tokens": 1000,
                    "cached_prompt_tokens": 800,
                    "completion_tokens": 200,
                })),
            ),
            row(
                &w,
                "up-2",
                "req-lk-4b7",
                unpriced,
                t0 + TimeDelta::seconds(1),
                tokens_680(),
            ),
            // A request that used nothing: billed nothing, which is not
            // pending.
            row(
                &w,
                "up-3",
                "req-lk-4b7",
                priced,
                t0 + TimeDelta::seconds(2),
                json!({}),
            ),
        ],
    )
    .await;
    let r = lookup(&w.dp, &w.key, "req-lk-4b7").await;
    let items = items(&r);
    assert_eq!(items.len(), 3, "{r:?}");
    assert_eq!(r.body["pending"], true);
    assert_eq!(r.body["total_usd"], "0.000680");
    assert_eq!(items[0]["charge_status"], "nothing_to_bill");
    assert_eq!(items[0]["charge_usd"], "0.000000");
    assert_eq!(items[1]["charge_status"], "pending");
    assert_eq!(items[1]["charge_usd"], Value::Null);
    assert_eq!(items[1]["quantities"], json!({}));
    assert_eq!(items[2]["charge_status"], "charged");
    assert_eq!(items[2]["charge_usd"], "0.000680");
}

#[tokio::test]
async fn an_adjusted_row_shows_its_corrected_cost() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let later = seed_model(&w.cp, &w.admin).await;
    let t0 = base(&w.cp).await;
    // 10 000 input tokens, 10 000 µ$ at `card()`; and a request on a model
    // with no price yet.
    send_all(
        &w,
        vec![
            row(
                &w,
                "adj-1",
                "req-lk-8a2",
                m,
                t0 + TimeDelta::seconds(1),
                json!({"prompt_tokens": 10_000}),
            ),
            row(&w, "adj-2", "req-lk-8a2", later, t0, tokens_680()),
        ],
    )
    .await;
    let r = lookup(&w.dp, &w.key, "req-lk-8a2").await;
    assert_eq!(items(&r)[0]["charge_usd"], "0.010000", "{r:?}");
    assert_eq!(r.body["pending"], true);

    // Input goes to 1.034 USD per million, so the first row costs 340 µ$
    // more; the second model gets `card()`, so a run charges its row 680 µ$.
    reprice_input(&w.cp, m, "1.034").await;
    price(&w.cp, later).await;
    run_repricing(&w.cp).await;
    let r = lookup(&w.dp, &w.key, "req-lk-8a2").await;
    let items = items(&r);
    assert_eq!(items[0]["charge_usd"], "0.010340", "{r:?}");
    assert_eq!(items[0]["charge_status"], "charged");
    // A row the run charged reads charged, as in the call log.
    assert_eq!(items[1]["charge_usd"], "0.000680", "{r:?}");
    assert_eq!(items[1]["charge_status"], "charged");
    // A run records no charge lines, so such a row shows no quantities.
    assert_eq!(items[1]["quantities"], json!({}));
    assert_eq!(r.body["total_usd"], "0.011020");
    assert_eq!(r.body["pending"], false);
}

#[tokio::test]
async fn the_request_lookup_names_no_channel() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let t0 = base(&w.cp).await;
    send_all(
        &w,
        vec![
            // A failover attempt the upstream billed, then the request failing
            // at the upstream with its own status and text.
            row(
                &w,
                "ch-1",
                "req-lk-1d8",
                m,
                t0,
                upstream_failure(json!({
                    "is_final_attempt": false,
                    "prompt_tokens": 1000,
                    "cached_prompt_tokens": 800,
                    "completion_tokens": 200,
                })),
            ),
            row(
                &w,
                "ch-2",
                "req-lk-1d8",
                m,
                t0 + TimeDelta::seconds(1),
                upstream_failure(json!({})),
            ),
        ],
    )
    .await;
    let r = lookup(&w.dp, &w.key, "req-lk-1d8").await;
    let items = items(&r);
    assert_eq!(items.len(), 2, "{r:?}");
    assert_eq!(items[0]["status"], "failed");
    assert_eq!(items[0]["error"], "upstream_unavailable");
    assert_eq!(items[1]["status"], "retried");
    assert_eq!(items[1]["error"], Value::Null);
    let body = r.body.to_string();
    for token in [
        PROVIDER_KEY,
        GROUP,
        UPSTREAM_MODEL,
        UPSTREAM_ERROR,
        "provider",
        "529",
        "cost_usd",
        &CLAIMED_COST.to_string(),
        "cost_micro_usd",
        "cost_source",
        "error_code",
        "error_class",
        "error_message",
        "status_code",
        "model_id",
        "attempt_model",
        "rate",
    ] {
        assert!(!body.contains(token), "{token} in {body}");
    }
    assert_eq!(keys_of(&r.body), set(&ANSWER_KEYS));
    for item in items {
        assert_eq!(keys_of(item), set(&ITEM_KEYS), "{item}");
    }
}

#[tokio::test]
async fn an_unacceptable_request_id_is_refused() {
    let Some(w) = world().await else { return };
    let refused_at_id = |r: &Resp| {
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(r.body["code"], "validation_failed", "{r:?}");
        assert_eq!(r.body["details"][0]["path"], "/id", "{r:?}");
    };
    let too_long = "a".repeat(257);
    for id in [
        too_long.as_str(),
        "req\u{1}x",
        "req\u{7f}x",
        "req x",
        "req-é",
        "",
    ] {
        refused_at_id(&lookup(&w.dp, &w.key, id).await);
    }
    // A malformed escape, and no id at all.
    refused_at_id(&lookup_raw(&w.dp, &w.key, "%FF").await);
    refused_at_id(
        &w.dp
            .send("GET", &format!("/dp/v1/keys/{}/requests", w.key), None)
            .await,
    );
    // The longest acceptable id, and every visible ASCII character, are
    // looked up: nothing is recorded under them.
    let longest = "a".repeat(256);
    let visible: String = (0x21u8..=0x7e).map(char::from).collect();
    for id in [longest.as_str(), visible.as_str()] {
        assert_eq!(lookup(&w.dp, &w.key, id).await.status, 404, "{id}");
    }

    // The browser listener does not serve it.
    let r =
        w.cp.send(
            "GET",
            &format!("/dp/v1/keys/{}/requests?id=x", w.key),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
}

#[tokio::test]
async fn a_request_id_with_reserved_characters_is_found() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let t0 = base(&w.cp).await;
    let id = "req/lk+5&a=b%c?d#e";
    send_all(&w, vec![row(&w, "rs-1", id, m, t0, tokens_680())]).await;
    let r = lookup(&w.dp, &w.key, id).await;
    assert_eq!(items(&r).len(), 1, "{r:?}");
    assert_eq!(r.body["request_id"], id);
}

/// A lookup whose query cannot finish within its statement timeout (here
/// one waiting on a table another transaction holds) gives its connection
/// back and answers `503` after that timeout, not the pool-wide one; the
/// next lookup is served.
#[tokio::test]
async fn a_lookup_past_its_statement_timeout_answers_503() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let t0 = base(&w.cp).await;
    send_all(&w, vec![row(&w, "to-1", "req-lk-5c9", m, t0, tokens_680())]).await;

    let mut holder = w.cp.db().pool().begin().await.expect("begin");
    sqlx::query("LOCK TABLE pricing_charges IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .expect("hold the charges");
    let started = std::time::Instant::now();
    let r = lookup(&w.dp, &w.key, "req-lk-5c9").await;
    let took = started.elapsed();
    holder.rollback().await.expect("release the charges");
    assert_eq!(
        (r.status, &r.body["code"]),
        (503, &json!("unavailable")),
        "{r:?}"
    );
    // The lookup's own 2 s, well before the 5 s lock timeout every other
    // statement has.
    assert!(took >= std::time::Duration::from_millis(1_900), "{took:?}");
    assert!(took < std::time::Duration::from_millis(4_000), "{took:?}");

    assert_eq!(items(&lookup(&w.dp, &w.key, "req-lk-5c9").await).len(), 1);
}

/// Intake stores a row's request id on its charge too, as the usage row
/// stores it, and so does the database for a writer that leaves it out; the
/// lookup and the call log find a charge by the id it carries, among its own
/// tenant's charges (`pricing_charges_tenant_request`).
#[tokio::test]
async fn a_charge_carries_its_rows_request_id() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let t0 = base(&w.cp).await;
    let mut none = row(&w, "rq-none", "unused", m, t0, tokens_680());
    none["request_id"] = Value::Null;
    let too_long = "r".repeat(257);
    send_all(
        &w,
        vec![
            row(&w, "rq-1", "req-lk-3e6", m, t0, tokens_680()),
            none,
            row(&w, "rq-long", &too_long, m, t0, tokens_680()),
        ],
    )
    .await;
    let stored: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT pc.event_id, pc.request_id, ue.request_id FROM pricing_charges pc \
         JOIN usage_events ue ON ue.event_id = pc.event_id ORDER BY pc.event_id",
    )
    .fetch_all(w.cp.db().pool())
    .await
    .expect("read the charges");
    let id = Some("req-lk-3e6".to_string());
    assert_eq!(
        stored,
        [
            ("rq-1".to_string(), id.clone(), id),
            ("rq-long".to_string(), None, None),
            ("rq-none".to_string(), None, None),
        ]
    );

    let defs: Vec<String> =
        sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE tablename = 'pricing_charges'")
            .fetch_all(w.cp.db().pool())
            .await
            .expect("read the indexes");
    assert!(
        defs.iter()
            .any(|d| d.contains("(tenant_id, request_id, occurred_at, seq)")),
        "{defs:?}"
    );

    // A charge an older writer inserts, one that predates the column and so
    // leaves it out, still carries its usage row's request id: both views
    // find it.
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, \
         request_id, event) \
         VALUES ('rq-raw', gen_random_uuid(), gen_random_uuid(), $1, $2::text, $3, 'req-lk-raw', \
                 '{\"is_final_attempt\": true, \"status_code\": 200}')",
    )
    .bind(t0)
    .bind(w.tenant)
    .bind(&w.key)
    .execute(w.cp.db().pool())
    .await
    .expect("insert the usage row");
    sqlx::query(
        "INSERT INTO pricing_charges (event_id, tenant_id, api_key_id, user_id, \
         requested_model_id, occurred_at, status, charge_micro_usd, engine_version) \
         VALUES ('rq-raw', $1::text, $2, $3::text, $4::text, $5, 'charged', 680, 1)",
    )
    .bind(w.tenant)
    .bind(&w.key)
    .bind(w.member)
    .bind(m)
    .bind(t0)
    .execute(w.cp.db().pool())
    .await
    .expect("insert the charge as an older writer does");
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let found = |n: usize| {
        let (w, owner) = (&w, &owner);
        async move {
            let calls =
                w.cp.send(
                    "GET",
                    "/api/usage/calls?request_id=req-lk-raw",
                    Some(owner.cookie()),
                    None,
                )
                .await;
            assert_eq!(calls.status, 200, "{calls:?}");
            assert_eq!(
                calls.body["items"].as_array().map(Vec::len),
                Some(n),
                "{calls:?}"
            );
            let r = lookup(&w.dp, &w.key, "req-lk-raw").await;
            if n == 0 {
                assert_eq!(r.status, 404, "{r:?}");
            } else {
                assert_eq!(items(&r).len(), n, "{r:?}");
            }
        }
    };
    found(1).await;

    // Both views go by the charge's own request id, not its usage row's.
    let set = |value: Option<&'static str>| {
        let w = &w;
        async move {
            sqlx::query("UPDATE pricing_charges SET request_id = $1 WHERE event_id = 'rq-raw'")
                .bind(value)
                .execute(w.cp.db().pool())
                .await
                .expect("set the charge's request id");
        }
    };
    set(None).await;
    found(0).await;
    set(Some("req-lk-raw")).await;
    found(1).await;
}

/// The lookup's statement timeout is its transaction's own: every pooled
/// connection, the one the lookup ran on among them, keeps the pool's
/// configured timeout afterwards, so intake on it is not cut short.
#[tokio::test]
async fn a_lookups_statement_timeout_ends_with_its_transaction() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let t0 = base(&w.cp).await;
    send_all(&w, vec![row(&w, "te-1", "req-lk-8d1", m, t0, tokens_680())]).await;
    assert_eq!(items(&lookup(&w.dp, &w.key, "req-lk-8d1").await).len(), 1);
    let pool = w.cp.db().pool();
    let idle = pool.num_idle();
    assert!(idle > 0, "no idle connection to check");
    let mut held = Vec::new();
    for _ in 0..idle {
        let mut c = pool.acquire().await.expect("acquire");
        let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&mut *c)
            .await
            .expect("show statement_timeout");
        assert_eq!(
            timeout, "30s",
            "a lookup's timeout outlived its transaction"
        );
        held.push(c);
    }
}

/// How many sessions of the test database wait on a lock now, read on
/// `conn` (which may hold a transaction open: the activity snapshot is
/// cleared first).
async fn sessions_waiting_on_a_lock(conn: &mut sqlx::PgConnection) -> i64 {
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut *conn)
        .await
        .expect("clear the activity snapshot");
    sqlx::query_scalar(
        "SELECT count(*)::bigint FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("count the waiting sessions")
}

/// The gateway lookups run on a pool of their own: however many lookups a
/// gateway sends and however long they take, they hold at most that pool's
/// connections, and a usage batch delivered meanwhile commits in its usual
/// time. Each lookup that finds no connection is refused within a second.
///
/// A request lookup reads the re-pricing adjustments, which intake never
/// touches; with that table locked, every lookup that has a connection
/// waits on it until its statement timeout.
#[tokio::test]
async fn stalled_lookups_never_hold_up_usage_intake() {
    use aisix_control_plane::billing::access::LOOKUP_POOL_SIZE;
    use std::time::{Duration, Instant};

    const LOOKUPS: usize = 12;
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", true).await;
    let t0 = base(&w.cp).await;
    send_all(
        &w,
        vec![row(&w, "pool-1", "req-lk-pool", m, t0, tokens_680())],
    )
    .await;

    let mut holder = w.cp.db().pool().begin().await.expect("begin");
    sqlx::query("LOCK TABLE pricing_adjustments IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .expect("hold the adjustments");

    let lookups = futures::future::join_all((0..LOOKUPS).map(|_| async {
        let started = Instant::now();
        let r = lookup(&w.dp, &w.key, "req-lk-pool").await;
        (r, started.elapsed())
    }));
    let intake = async {
        // Once the lookups hold connections and wait on the lock, deliver.
        let mut most = 0;
        while most < i64::from(LOOKUP_POOL_SIZE) {
            most = most.max(sessions_waiting_on_a_lock(&mut holder).await);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let started = Instant::now();
        send_all(
            &w,
            vec![row(&w, "pool-2", "req-lk-pool-2", m, t0, tokens_680())],
        )
        .await;
        let took = started.elapsed();
        most = most.max(sessions_waiting_on_a_lock(&mut holder).await);
        (took, most)
    };
    let (answers, (intake_took, most_waiting)) = tokio::join!(lookups, intake);
    holder.rollback().await.expect("release the adjustments");

    assert!(
        intake_took < Duration::from_secs(1),
        "the batch took {intake_took:?} while the lookups were stalled"
    );
    assert_eq!(
        most_waiting,
        i64::from(LOOKUP_POOL_SIZE),
        "lookups holding a connection"
    );
    for (r, took) in &answers {
        assert_eq!(
            (r.status, &r.body["code"]),
            (503, &json!("unavailable")),
            "{r:?}"
        );
        assert!(*took < Duration::from_secs(3), "a lookup took {took:?}");
    }

    // Released, the lookups are served again, and the batch was stored.
    assert_eq!(
        items(&lookup(&w.dp, &w.key, "req-lk-pool-2").await).len(),
        1
    );
}
