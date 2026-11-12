//! A tenant's call log and usage summary: the priced usage rows of its own
//! keys, with no channel information, no cost price and no raw upstream
//! status or error. A member reads only the calls of their own keys.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it.

mod support;

use std::collections::BTreeSet;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use support::members::MemberSession;
use support::world::{deliver, price, seed_key, seed_tenant, tokens_680, usage, world, World};
use support::{Resp, TestCp};
use uuid::Uuid;

const PROVIDER_KEY: &str = "acct-bill-7c1";
const GROUP: &str = "grp-bill-5d2";
const UPSTREAM_MODEL: &str = "upstream-mdl-9f3";
const UPSTREAM_ERROR: &str = "upstream secret 4b7";

/// The keys of a `Call`, from the API section.
const CALL_KEYS: [&str; 16] = [
    "event_id",
    "occurred_at",
    "request_id",
    "model",
    "api_key",
    "member",
    "operation",
    "status",
    "status_code",
    "error_class",
    "cache_hit",
    "guardrail_blocked",
    "latency_ms",
    "first_token_latency_ms",
    "attempt_duration_ms",
    "charge",
];

/// The keys of a `Call`'s `charge`.
const CHARGE_KEYS: [&str; 9] = [
    "status",
    "amount_micro_cny",
    "group",
    "adjusted_micro_cny",
    "service_tier",
    "estimated",
    "discount_bps",
    "lines",
    "calculation",
];

/// The keys of one line of a charge, as a tenant reads it.
const LINE_KEYS: [&str; 3] = ["dimension", "quantity", "rate_cny"];

/// A model named `name` for tenants, on a provider key and groups that carry
/// the channel fixture tokens; granted to `tenant` when given.
async fn model(w: &World, name: &str, grant_to: Option<Uuid>) -> Uuid {
    support::seed_group(&w.cp, &w.admin, GROUP).await;
    let pk =
        w.cp.send(
            "POST",
            "/admin/resources/provider_keys",
            Some(w.admin.cookie()),
            Some(json!({"doc": {
                "display_name": format!("{PROVIDER_KEY}-{name}"),
                "provider": "openai",
                "api_key": "sk-views-upstream-7c1",
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
    price(&w.cp, id).await;
    if let Some(tenant) = grant_to {
        grant(&w.cp, tenant, id).await;
    }
    id
}

async fn grant(cp: &TestCp, tenant: Uuid, model: Uuid) {
    sqlx::query(
        "UPDATE tenants SET allowed_model_ids = array_append(allowed_model_ids, $2) WHERE id = $1",
    )
    .bind(tenant)
    .bind(model)
    .execute(cp.db().pool())
    .await
    .expect("grant the model");
}

/// A usage row as the gateway records a request's last attempt, carrying
/// the channel fields a tenant must never see.
fn row(w: &World, id: &str, model: Uuid, name: &str, fields: Value) -> Value {
    let mut fields = fields;
    let extra = json!({
        "is_final_attempt": true,
        "requested_model": name,
        "model_id": model.to_string(),
        "attempt_model": UPSTREAM_MODEL,
        "provider_key_name": PROVIDER_KEY,
        "provider_group": GROUP,
    });
    for (k, v) in extra.as_object().expect("an object") {
        if fields.get(k).is_none() {
            fields[k] = v.clone();
        }
    }
    usage(w, id, model, fields)
}

fn failure(status: u16, class: &str, message: &str) -> Value {
    json!({"status_code": status, "error_class": class, "error_message": message})
}

async fn send_all(w: &World, events: Vec<Value>) {
    let r = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(r.status, 200, "{r:?}");
}

async fn get(cp: &TestCp, path: &str, by: &MemberSession) -> Resp {
    cp.send("GET", path, Some(by.cookie()), None).await
}

fn items(r: &Resp) -> &Vec<Value> {
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items")
}

fn event_ids(r: &Resp) -> BTreeSet<String> {
    items(r)
        .iter()
        .map(|c| c["event_id"].as_str().expect("an event id").to_string())
        .collect()
}

fn ids(of: &[&str]) -> BTreeSet<String> {
    of.iter().map(|s| s.to_string()).collect()
}

async fn owner(w: &World) -> MemberSession {
    w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await
}

const WINDOW: &str = "group=day&from=2020-01-01T00:00:00Z&to=2020-03-01T00:00:00Z";

/// A summary window around now, for rows that occur now.
fn around_now() -> String {
    let now = chrono::Utc::now();
    format!(
        "group=day&from={}&to={}",
        (now - chrono::TimeDelta::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        (now + chrono::TimeDelta::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

#[tokio::test]
async fn a_tenants_call_log_names_no_channel_and_no_cost() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    send_all(
        &w,
        vec![
            row(&w, "ok1", m, "chat-bill", tokens_680()),
            row(
                &w,
                "bad1",
                m,
                "chat-bill",
                failure(500, "upstream_status", UPSTREAM_ERROR),
            ),
        ],
    )
    .await;
    // A later engine may store more on a charge line than a tenant reads,
    // such as a line's cost rate: written here as such an engine would.
    let stored = sqlx::query(
        "UPDATE pricing_charges_cny SET lines = (SELECT jsonb_agg(l || $1::jsonb) \
         FROM jsonb_array_elements(lines) l) \
         WHERE event_id = 'ok1' AND jsonb_array_length(lines) > 0",
    )
    .bind(json!({"cost_rate": "0.4", "cost_revision_id": 9_301}))
    .execute(w.cp.db().pool())
    .await
    .expect("store a later engine's lines");
    assert_eq!(
        stored.rows_affected(),
        1,
        "the charge has no lines to extend"
    );
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls", &o).await;
    assert_eq!(event_ids(&r), ids(&["ok1", "bad1"]));
    let body = r.body.to_string();
    for token in [
        PROVIDER_KEY,
        GROUP,
        UPSTREAM_MODEL,
        UPSTREAM_ERROR,
        "cost_usd",
        "error_code",
        "cost_rate",
        "cost_revision_id",
    ] {
        assert!(!body.contains(token), "{token} in {body}");
    }
    let want: BTreeSet<String> = CALL_KEYS.iter().map(|k| k.to_string()).collect();
    let keys = |v: &Value| -> BTreeSet<String> {
        v.as_object().expect("an object").keys().cloned().collect()
    };
    let charge_keys: BTreeSet<String> = CHARGE_KEYS.iter().map(|k| k.to_string()).collect();
    let line_keys: BTreeSet<String> = LINE_KEYS.iter().map(|k| k.to_string()).collect();
    let mut lines_seen = 0;
    for item in items(&r) {
        assert_eq!(keys(item), want, "{item}");
        assert_eq!(keys(&item["charge"]), charge_keys, "{item}");
        for line in item["charge"]["lines"].as_array().expect("lines") {
            assert_eq!(keys(line), line_keys, "{line}");
            lines_seen += 1;
        }
    }
    assert!(lines_seen > 0, "no charge line was checked: {body}");
    let summary = get(&w.cp, &format!("/api/usage/summary?{}", around_now()), &o).await;
    let body = summary.body.to_string();
    for token in [
        PROVIDER_KEY,
        GROUP,
        UPSTREAM_MODEL,
        UPSTREAM_ERROR,
        "cost_usd",
    ] {
        assert!(!body.contains(token), "{token} in {body}");
    }
}

#[tokio::test]
async fn a_forbidden_models_call_reads_exactly_like_an_unknown_models() {
    let Some(w) = world().await else { return };
    // Exists, priced, but not granted to the tenant.
    let hidden = model(&w, "chat-hidden", None).await;
    send_all(
        &w,
        vec![
            row(
                &w,
                "forbidden",
                hidden,
                "chat-hidden",
                failure(
                    403,
                    "permission_denied",
                    "API key is not allowed to use model \"chat-hidden\"",
                ),
            ),
            row(&w, "unknown", hidden, "no-such-model-3a1", {
                let mut f = failure(404, "model_not_found", "model not found");
                f["requested_model_id"] = json!("");
                f["model_id"] = json!("");
                f
            }),
        ],
    )
    .await;
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls", &o).await;
    let by_id = |id: &str| -> Value {
        items(&r)
            .iter()
            .find(|c| c["event_id"] == id)
            .unwrap_or_else(|| panic!("{id} in {:?}", r.body))
            .clone()
    };
    for id in ["forbidden", "unknown"] {
        let c = by_id(id);
        assert_eq!(c["status"], "failed", "{c}");
        assert_eq!(c["status_code"], Value::Null, "{c}");
        assert_eq!(c["error_class"], "model_not_found", "{c}");
        assert_eq!(c["model"]["id"], Value::Null, "{c}");
    }
}

#[tokio::test]
async fn a_failed_failover_attempt_shows_no_upstream_status() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    let mut a1 = row(
        &w,
        "r1a1",
        m,
        "chat-bill",
        failure(529, "upstream_status", UPSTREAM_ERROR),
    );
    a1["request_id"] = json!("r1");
    a1["is_final_attempt"] = json!(false);
    let mut a2 = row(&w, "r1a2", m, "chat-bill", tokens_680());
    a2["request_id"] = json!("r1");
    let mut r2 = row(
        &w,
        "r2a1",
        m,
        "chat-bill",
        failure(529, "upstream_status", UPSTREAM_ERROR),
    );
    r2["request_id"] = json!("r2");
    send_all(&w, vec![a1, a2, r2]).await;
    let o = owner(&w).await;

    let r = get(&w.cp, "/api/usage/calls?request_id=r1", &o).await;
    assert_eq!(items(&r).len(), 1, "{r:?}");
    assert_eq!(items(&r)[0]["status"], "succeeded");
    assert_eq!(items(&r)[0]["status_code"], 200);

    let r = get(&w.cp, "/api/usage/calls?request_id=r2", &o).await;
    assert_eq!(items(&r).len(), 1, "{r:?}");
    assert_eq!(items(&r)[0]["status"], "failed");
    assert_eq!(items(&r)[0]["status_code"], Value::Null);
    assert_eq!(items(&r)[0]["error_class"], "upstream_unavailable");

    let all = get(&w.cp, "/api/usage/calls", &o).await;
    assert!(
        items(&all).iter().all(|c| c["status_code"] != 529),
        "{all:?}"
    );

    // The hidden attempt is no request of the summary either: two requests
    // (r1's final attempt and r2), not three.
    let s = get(&w.cp, &format!("/api/usage/summary?{}", around_now()), &o).await;
    let requests: i64 = items(&s)
        .iter()
        .map(|i| i["requests"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(requests, 2, "{s:?}");
}

#[tokio::test]
async fn hidden_attempts_do_not_shorten_a_page() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    // Two visible rows first, then three hidden attempts newer than both.
    send_all(
        &w,
        vec![
            row(&w, "v1", m, "chat-bill", json!({})),
            row(&w, "v2", m, "chat-bill", json!({})),
        ],
    )
    .await;
    for id in ["h1", "h2", "h3"] {
        let mut a = row(
            &w,
            id,
            m,
            "chat-bill",
            failure(529, "upstream_status", UPSTREAM_ERROR),
        );
        a["is_final_attempt"] = json!(false);
        send_all(&w, vec![a]).await;
    }
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls?limit=2", &o).await;
    assert_eq!(event_ids(&r), ids(&["v1", "v2"]), "{r:?}");
    assert_eq!(r.body["next_before"], Value::Null, "{r:?}");
}

#[tokio::test]
async fn a_stream_that_broke_after_a_200_is_a_failed_call() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    send_all(
        &w,
        vec![row(
            &w,
            "broke",
            m,
            "chat-bill",
            failure(200, "stream_aborted", UPSTREAM_ERROR),
        )],
    )
    .await;
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls", &o).await;
    let c = &items(&r)[0];
    assert_eq!(c["status"], "failed", "{c}");
    assert_eq!(c["status_code"], Value::Null);
    assert_eq!(c["error_class"], "upstream_unavailable");
}

#[tokio::test]
async fn a_summary_groups_by_model_id_not_by_the_name_sent() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    // Many distinct bogus names, none resolving to a model, and an
    // ungranted model: one row with no id and no name.
    let hidden = model(&w, "chat-hidden", None).await;
    let mut events = vec![row(&w, "g0", m, "chat-bill", json!({}))];
    for i in 0..5 {
        let mut f = failure(404, "model_not_found", "model not found");
        f["requested_model_id"] = json!("");
        events.push(row(
            &w,
            &format!("bogus{i}"),
            m,
            &format!("bogus-name-{i}"),
            f,
        ));
    }
    events.push(row(&w, "forb", hidden, "chat-hidden", json!({})));
    send_all(&w, events).await;
    let o = owner(&w).await;
    let r = get(&w.cp, &format!("/api/usage/summary?{}", around_now()), &o).await;
    let rows = items(&r);
    assert_eq!(rows.len(), 2, "{r:?}");
    let granted = rows
        .iter()
        .find(|i| i["model"]["id"] == m.to_string())
        .expect("granted");
    assert_eq!(granted["model"]["name"], "chat-bill");
    assert_eq!(granted["requests"], 1);
    let rest = rows
        .iter()
        .find(|i| i["model"]["id"].is_null())
        .expect("the rest");
    assert_eq!(rest["model"]["name"], "");
    assert_eq!(rest["requests"], 6);
    assert!(!r.body.to_string().contains("bogus-name"), "{r:?}");
}

#[tokio::test]
async fn a_charged_retried_attempt_is_listed_as_retried() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    let mut attempt = row(&w, "billed1", m, "chat-bill", tokens_680());
    attempt["is_final_attempt"] = json!(false);
    send_all(&w, vec![attempt]).await;
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls", &o).await;
    assert_eq!(items(&r).len(), 1, "{r:?}");
    let c = &items(&r)[0];
    assert_eq!(c["status"], "retried", "{c}");
    assert_eq!(c["status_code"], Value::Null);
    assert_eq!(c["error_class"], Value::Null);
    assert_eq!(c["charge"]["status"], "charged");
    assert_eq!(c["charge"]["amount_micro_cny"], 680);
}

#[tokio::test]
async fn a_member_sees_only_their_own_keys_and_billing_sees_all() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    let mem = w.cp.seed_member(w.tenant, MemberRole::Member).await;
    let mem_key = seed_key(&w.cp, &w.admin, w.tenant, Some(mem.id)).await;
    let own = w.cp.seed_member(w.tenant, MemberRole::Owner).await;
    let own_key = seed_key(&w.cp, &w.admin, w.tenant, Some(own.id)).await;
    let mut events = Vec::new();
    for (id, key, who) in [
        ("m1", &mem_key, mem.id),
        ("m2", &mem_key, mem.id),
        ("o1", &own_key, own.id),
        ("o2", &own_key, own.id),
        ("o3", &own_key, own.id),
    ] {
        events.push(row(
            &w,
            id,
            m,
            "chat-bill",
            json!({"api_key_id": key, "user_id": who.to_string(), "prompt_tokens": 10}),
        ));
    }
    send_all(&w, events).await;

    let as_member = w.cp.member_login_as(&mem).await;
    let r = get(&w.cp, "/api/usage/calls", &as_member).await;
    assert_eq!(event_ids(&r), ids(&["m1", "m2"]));
    let s = get(
        &w.cp,
        &format!("/api/usage/summary?{}", around_now()),
        &as_member,
    )
    .await;
    let requests: i64 = items(&s)
        .iter()
        .map(|i| i["requests"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(requests, 2, "{s:?}");

    let billing =
        w.cp.member_login_seeded(w.tenant, MemberRole::Billing)
            .await;
    let r = get(&w.cp, "/api/usage/calls", &billing).await;
    assert_eq!(event_ids(&r), ids(&["m1", "m2", "o1", "o2", "o3"]));
    let s = get(
        &w.cp,
        &format!("/api/usage/summary?{}", around_now()),
        &billing,
    )
    .await;
    let requests: i64 = items(&s)
        .iter()
        .map(|i| i["requests"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(requests, 5, "{s:?}");
}

#[tokio::test]
async fn the_call_log_filters_by_request_id() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    let mut events = Vec::new();
    for (id, req) in [("a", "req-v-7c1"), ("b", "req-v-7c1"), ("c", "req-v-9f3")] {
        let mut e = row(&w, id, m, "chat-bill", json!({}));
        e["request_id"] = json!(req);
        events.push(e);
    }
    send_all(&w, events).await;
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls?request_id=req-v-7c1", &o).await;
    assert_eq!(event_ids(&r), ids(&["a", "b"]));
    // A bind parameter, never a pattern.
    let r = get(&w.cp, "/api/usage/calls?request_id=req-v-%25", &o).await;
    assert!(items(&r).is_empty(), "{r:?}");
}

#[tokio::test]
async fn rows_filed_under_another_tenant_never_appear() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    let b_tenant = seed_tenant(&w.cp, &w.admin).await;
    let b_member = w.cp.seed_member(b_tenant, MemberRole::Owner).await;
    let b_key = seed_key(&w.cp, &w.admin, b_tenant, Some(b_member.id)).await;
    // Names tenant A, with B's key: filed under B.
    let stray = row(
        &w,
        "stray",
        m,
        "chat-bill",
        json!({"api_key_id": b_key, "user_id": b_member.id.to_string()}),
    );
    let own = row(&w, "own", m, "chat-bill", json!({}));
    send_all(&w, vec![stray, own]).await;

    let a = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls", &a).await;
    assert_eq!(event_ids(&r), ids(&["own"]));
    let b = w.cp.member_login_as(&b_member).await;
    let r = get(&w.cp, "/api/usage/calls", &b).await;
    assert_eq!(event_ids(&r), ids(&["stray"]));
}

#[tokio::test]
async fn beijing_usage_summaries_bucket_counts_money_and_quantities_across_the_year() {
    let Some(w) = world().await else { return };
    let m = model(&w, "beijing-summary", Some(w.tenant)).await;
    let boundary = DateTime::parse_from_rfc3339("2025-12-31T16:00:00Z")
        .expect("北京元旦午夜")
        .with_timezone(&Utc);
    support::world::insert_baseline_backdated(
        w.cp.db(),
        m,
        serde_json::to_value(support::world::card()).expect("价格卡"),
        boundary - TimeDelta::days(2),
    )
    .await;
    let from = boundary - TimeDelta::seconds(1);
    let to = boundary + TimeDelta::days(1);
    let mut events = Vec::new();
    for (id, at) in [
        ("beijing-before-from", from - TimeDelta::microseconds(1)),
        ("beijing-from", from),
        ("beijing-midnight", boundary),
        ("beijing-after", boundary + TimeDelta::seconds(1)),
        ("beijing-to", to),
    ] {
        let mut e = row(&w, id, m, "beijing-summary", tokens_680());
        e["occurred_at"] = json!(at);
        events.push(e);
    }
    send_all(&w, events).await;
    let owner = owner(&w).await;
    for (group, starts) in [
        ("day", ["2025-12-30T16:00:00Z", "2025-12-31T16:00:00Z"]),
        ("month", ["2025-11-30T16:00:00Z", "2025-12-31T16:00:00Z"]),
    ] {
        let query = format!(
            "group={group}&from={}&to={}",
            from.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            to.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        );
        for (path, cookie) in [
            ("/api/usage/summary".to_string(), owner.cookie()),
            (
                format!("/admin/tenants/{}/usage/summary", w.tenant),
                w.admin.cookie(),
            ),
        ] {
            let r =
                w.cp.send("GET", &format!("{path}?{query}"), Some(cookie), None)
                    .await;
            let rows = items(&r);
            assert_eq!(rows.len(), 2, "北京跨年必须有两个{group}桶：{r:?}");
            for (index, requests) in [1, 2].into_iter().enumerate() {
                assert_eq!(rows[index]["period_start"], starts[index], "{path} {group}");
                assert_eq!(rows[index]["requests"], requests, "{path} {group}");
                assert_eq!(
                    rows[index]["charged_micro_cny"],
                    requests * 680,
                    "{path} {group}"
                );
                assert_eq!(
                    rows[index]["quantities"],
                    json!({"input": requests * 200, "output": requests * 200, "cache_read": requests * 800}),
                    "数量SQL必须与费用SQL使用相同北京桶：{path} {group}"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_usage_summary_counts_requests_charges_and_quantities_by_model_and_day() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    let m2 = model(&w, "chat-bill-2", Some(w.tenant)).await;
    send_all(
        &w,
        vec![
            row(&w, "s1", m, "chat-bill", tokens_680()),
            row(&w, "s2", m, "chat-bill", tokens_680()),
            row(&w, "s3", m, "chat-bill", tokens_680()),
            row(&w, "s4", m2, "chat-bill-2", tokens_680()),
        ],
    )
    .await;
    let o = owner(&w).await;
    let r = get(&w.cp, &format!("/api/usage/summary?{}", around_now()), &o).await;
    let rows = items(&r);
    assert_eq!(rows.len(), 2, "{r:?}");
    let of = |name: &str| {
        rows.iter()
            .find(|i| i["model"]["name"] == name)
            .expect(name)
    };
    let first = of("chat-bill");
    assert_eq!(first["model"]["id"], m.to_string());
    assert_eq!(first["requests"], 3);
    assert_eq!(first["charged_micro_cny"], 2_040);
    assert_eq!(first["estimated_requests"], 0);
    // 1000 prompt tokens, 800 of them cache hits, and 200 completion tokens.
    assert_eq!(first["quantities"]["input"], 600);
    assert_eq!(first["quantities"]["cache_read"], 2_400);
    assert_eq!(first["quantities"]["output"], 600);
    let second = of("chat-bill-2");
    assert_eq!(second["model"]["id"], m2.to_string());
    assert_eq!(second["requests"], 1);
    assert_eq!(second["charged_micro_cny"], 680);
    assert_eq!(first["period_start"], second["period_start"]);

    // The range is bounded and well-formed.
    let r = get(
        &w.cp,
        &format!("/api/usage/summary?{WINDOW}&api_key_id=nope"),
        &o,
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    let r = get(
        &w.cp,
        "/api/usage/summary?group=day&from=2020-01-01T00:00:00Z&to=2021-01-01T00:00:00Z",
        &o,
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/to");
}

#[tokio::test]
async fn operators_read_any_tenants_usage_summary() {
    let Some(w) = world().await else { return };
    let m = model(&w, "chat-bill", Some(w.tenant)).await;
    send_all(&w, vec![row(&w, "v1", m, "chat-bill", tokens_680())]).await;
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let r =
        w.cp.send(
            "GET",
            &format!("/admin/tenants/{}/usage/summary?{}", w.tenant, around_now()),
            Some(viewer.cookie()),
            None,
        )
        .await;
    assert_eq!(items(&r).len(), 1, "{r:?}");
    assert_eq!(items(&r)[0]["requests"], 1);
    assert_eq!(items(&r)[0]["charged_micro_cny"], 680);
    let r =
        w.cp.send(
            "GET",
            &format!(
                "/admin/tenants/{}/usage/summary?{}",
                Uuid::new_v4(),
                around_now()
            ),
            Some(viewer.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
}

#[tokio::test]
async fn delivered_first_token_is_nullable_and_never_inferred_from_first_frame() {
    let Some(w) = world().await else { return };
    let m = model(&w, "timed-call", Some(w.tenant)).await;
    send_all(
        &w,
        vec![
            row(
                &w,
                "timed",
                m,
                "timed-call",
                json!({
                    "first_token_latency_ms": 42, "upstream_ttft_ms": 2,
                    "upstream_latency_ms": 90, "downstream_latency_ms": 8,
                }),
            ),
            row(
                &w,
                "legacy",
                m,
                "timed-call",
                json!({
                    "upstream_ttft_ms": 3, "downstream_latency_ms": 9,
                }),
            ),
            row(
                &w,
                "no-content",
                m,
                "timed-call",
                json!({
                    "first_token_latency_ms": null, "upstream_ttft_ms": 4,
                }),
            ),
        ],
    )
    .await;
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls", &o).await;
    let of = |id: &str| {
        items(&r)
            .iter()
            .find(|c| c["event_id"] == id)
            .expect("call")
    };
    assert_eq!(of("timed")["first_token_latency_ms"], 42);
    assert_eq!(of("timed")["attempt_duration_ms"], 90);
    assert_eq!(of("timed")["latency_ms"], 8);
    for id in ["legacy", "no-content"] {
        assert_eq!(of(id).get("first_token_latency_ms"), Some(&Value::Null));
        assert_eq!(of(id).get("attempt_duration_ms"), Some(&Value::Null));
    }
}

#[tokio::test]
async fn historical_charge_calculation_is_exact_across_pages_and_repricing() {
    use aisix_pricing::RateCard;
    use support::world::{db_now, insert_baseline};
    let Some(w) = world().await else { return };
    let m = model(&w, "formula-call", Some(w.tenant)).await;
    send_all(
        &w,
        vec![
            row(&w, "formula1", m, "formula-call", tokens_680()),
            row(&w, "formula2", m, "formula-call", tokens_680()),
        ],
    )
    .await;
    let o = owner(&w).await;
    let first = get(&w.cp, "/api/usage/calls?limit=1", &o).await;
    let before = first.body["next_before"].as_i64().expect("next page");
    let second = get(
        &w.cp,
        &format!("/api/usage/calls?limit=1&before={before}"),
        &o,
    )
    .await;
    for page in [&first, &second] {
        assert_eq!(
            items(page)[0]["charge"]["calculation"],
            json!({
                "original_micro_cny": "680", "adjusted_micro_cny": "0",
                "final_micro_cny": "680", "rounding": "ceil_micro_cny",
            })
        );
    }
    let card: RateCard = serde_json::from_value(json!({"default":{"base":{
        "input":"2", "output":"4", "cache_read":"0.2"
    }}}))
    .expect("price card");
    insert_baseline(&w.cp, m, db_now(&w.cp).await, card)
        .await
        .expect("future price");
    let unchanged = get(&w.cp, "/api/usage/calls", &o).await;
    assert!(items(&unchanged)
        .iter()
        .all(|c| c["charge"]["calculation"]["final_micro_cny"] == "680"));
    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let now = db_now(&w.cp).await;
    let preview =
        w.cp.send(
            "POST",
            "/admin/billing/repricing",
            Some(w.admin.cookie()),
            Some(json!({
                "from": now - chrono::TimeDelta::hours(1), "to": now,
                "tenant_id": w.tenant, "reason": "验证历史费用公式",
            })),
        )
        .await;
    assert_eq!(preview.status, 201, "{preview:?}");
    let run = preview.body["id"].as_str().expect("run id");
    let applied =
        w.cp.send(
            "POST",
            &format!("/admin/billing/repricing/{run}/apply"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(applied.status, 200, "{applied:?}");
    let corrected = get(&w.cp, "/api/usage/calls", &o).await;
    for c in items(&corrected) {
        assert_eq!(
            c["charge"]["calculation"],
            json!({
                "original_micro_cny": "680", "adjusted_micro_cny": "680",
                "final_micro_cny": "1360", "rounding": "ceil_micro_cny",
            })
        );
        let lines = c["charge"]["lines"].as_array().expect("lines");
        assert_eq!(
            lines
                .iter()
                .find(|l| l["dimension"] == "input")
                .expect("input")["rate_cny"],
            "2"
        );
    }
}

#[tokio::test]
async fn charge_calculation_keeps_full_i64_precision_and_unknown_amounts() {
    let Some(w) = world().await else { return };
    let m = model(&w, "precise-call", Some(w.tenant)).await;
    send_all(
        &w,
        vec![row(&w, "precise", m, "precise-call", tokens_680())],
    )
    .await;
    // 金额边界是存储合同测试；不以浏览器的浮点数作为期望。
    sqlx::query("UPDATE pricing_charges_cny SET charge_micro_cny = 9007199254740993 WHERE event_id = 'precise'")
        .execute(w.cp.db().pool()).await.expect("i64 boundary");
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls", &o).await;
    assert_eq!(
        items(&r)[0]["charge"]["calculation"]["original_micro_cny"],
        "9007199254740993"
    );
    assert_eq!(
        items(&r)[0]["charge"]["calculation"]["final_micro_cny"],
        "9007199254740993"
    );
    sqlx::query("UPDATE pricing_charges_cny SET status = 'unpriced', charge_micro_cny = NULL WHERE event_id = 'precise'")
        .execute(w.cp.db().pool()).await.expect("unpriced boundary");
    let r = get(&w.cp, "/api/usage/calls", &o).await;
    assert_eq!(
        items(&r)[0]["charge"]["calculation"].get("final_micro_cny"),
        Some(&Value::Null)
    );
}

#[tokio::test]
async fn a_disconnected_billable_call_explains_its_safe_failure_reason() {
    let Some(w) = world().await else { return };
    let m = model(&w, "disconnected-call", Some(w.tenant)).await;
    let mut fields = tokens_680();
    fields["status_code"] = json!(499);
    fields["error_class"] = json!("client_disconnected");
    fields["error_message"] = json!("private-account.internal secret credential");
    send_all(
        &w,
        vec![row(&w, "disconnected", m, "disconnected-call", fields)],
    )
    .await;
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls", &o).await;
    let c = &items(&r)[0];
    assert_eq!(c["status"], "failed");
    assert_eq!(c["error_class"], "client_disconnected");
    assert_eq!(c["charge"]["amount_micro_cny"], 680);
    assert!(!r.body.to_string().contains("private-account"));
    assert!(!r.body.to_string().contains("credential"));
}

#[tokio::test]
async fn historical_formula_retains_fractional_rates_and_ceil_applies_once_after_discount() {
    use aisix_control_plane::audit::Actor;
    use aisix_control_plane::pricing::store::{self, NewDiscount};
    use aisix_pricing::RateCard;
    use futures::FutureExt;
    use support::world::{db_now, insert_baseline};
    let Some(w) = world().await else { return };
    let m = model(&w, "rounding-call", Some(w.tenant)).await;
    let card: RateCard = serde_json::from_value(json!({"default":{"base":{
        "input":"0.4", "output":"0.4", "cache_read":"0.4"
    }}}))
    .expect("fractional price card");
    insert_baseline(&w.cp, m, db_now(&w.cp).await, card)
        .await
        .expect("fractional price");
    let tenant = w.tenant;
    let effective_from = db_now(&w.cp).await;
    w.cp.db()
        .tx(move |c| {
            async move {
                store::insert_discount(
                    c,
                    &NewDiscount {
                        tenant_id: tenant,
                        effective_from,
                        discount_bps: 1000,
                    },
                    Actor::System,
                )
                .await
            }
            .boxed()
        })
        .await
        .expect("ten percent discount");
    send_all(
        &w,
        vec![row(
            &w,
            "rounding",
            m,
            "rounding-call",
            json!({
                "prompt_tokens":1, "completion_tokens":1,
            }),
        )],
    )
    .await;
    let o = owner(&w).await;
    let r = get(&w.cp, "/api/usage/calls", &o).await;
    let c = &items(&r)[0]["charge"];
    assert_eq!(c["discount_bps"], 1000);
    assert_eq!(c["calculation"]["final_micro_cny"], "1");
    let lines = c["lines"].as_array().expect("historical lines");
    assert_eq!(lines.len(), 2);
    for l in lines {
        assert_eq!(l["quantity"], 1);
        assert_eq!(l["rate_cny"], "0.4");
    }
}
