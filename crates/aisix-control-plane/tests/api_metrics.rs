//! 全量指标走真实 HTTP、遥测入口和独立 Postgres 数据库。

mod support;

use aisix_control_plane::auth::MemberRole;
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde_json::{json, Value};
use support::world::{deliver, price, seed_key, seed_model, seed_tenant, usage, world, World};
use support::Resp;
use uuid::Uuid;

async fn intake(w: &World, events: Vec<Value>) {
    for batch in events.chunks(500) {
        let response = deliver(&w.dp, Uuid::new_v4(), batch.to_vec()).await;
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(response.body["rejected"], 0, "{response:?}");
    }
}

fn event(w: &World, id: &str, model: Uuid, extra: Value) -> Value {
    let mut row = usage(w, id, model, extra);
    row["is_final_attempt"] = json!(true);
    row
}

fn totals(response: &Resp) -> &Value {
    assert_eq!(response.status, 200, "{response:?}");
    assert!(response.body["totals"].is_object(), "{response:?}");
    &response.body["totals"]
}

fn near(value: &Value, expected: f64) {
    let actual = value
        .as_f64()
        .unwrap_or_else(|| panic!("缺失数值: {value}"));
    assert!(
        (actual - expected).abs() < 0.000001,
        "{actual} != {expected}"
    );
}

#[tokio::test]
async fn final_outcomes_real_ttft_mixed_cache_and_billed_retries_are_separate() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let mut retry = event(
        &w,
        "retry",
        model,
        json!({
            "prompt_tokens": 100, "completion_tokens": 10, "cached_prompt_tokens": 80,
            "status_code": 500, "upstream_latency_ms": 300,
        }),
    );
    retry["is_final_attempt"] = json!(false);
    retry["request_id"] = json!("reused-client-id");
    let mut success = event(
        &w,
        "success",
        model,
        json!({
            "prompt_tokens": 100, "completion_tokens": 20, "reasoning_tokens": 5,
            "cache_read_tokens": 50, "cache_creation_tokens": 50,
            "first_token_latency_ms": 100, "downstream_latency_ms": 90,
            "upstream_latency_ms": 500, "cache_status": "hit",
        }),
    );
    success["request_id"] = json!("reused-client-id");
    let mut broken = event(
        &w,
        "broken",
        model,
        json!({
            "prompt_tokens": 100, "completion_tokens": 10, "cached_prompt_tokens": 20,
            "error_class": "stream_aborted", "first_token_latency_ms": 200,
            "downstream_latency_ms": 180, "upstream_latency_ms": 700,
            "cache_status": "miss",
        }),
    );
    broken["request_id"] = json!("reused-client-id");
    let empty = event(
        &w,
        "nonstream",
        model,
        json!({"downstream_latency_ms": 30,
        "upstream_latency_ms": 30, "cache_status": "disabled"}),
    );
    intake(&w, vec![retry, success, broken, empty]).await;
    let r =
        w.cp.send("GET", "/api/usage/metrics", Some(owner.cookie()), None)
            .await;
    let t = totals(&r);
    assert_eq!(t["requests"], 3);
    assert_eq!(t["succeeded_requests"], 2);
    assert_eq!(t["failed_requests"], 1);
    near(&t["success_rate"], 2.0 / 3.0);
    assert_eq!(t["first_token_latency_ms"]["sample_count"], 2);
    assert_eq!(t["first_token_latency_ms"]["missing_count"], 1);
    near(&t["first_token_latency_ms"]["avg"], 150.0);
    near(&t["first_token_latency_ms"]["p50"], 150.0);
    near(&t["first_token_latency_ms"]["p95"], 195.0);
    near(&t["response_latency_ms"]["avg"], 100.0);
    assert_eq!(t["attempt_duration_ms"]["sample_count"], 4);
    assert_eq!(t["billing_attempts"], 4);
    assert_eq!(t["tokens"]["prompt_tokens"], 300);
    assert_eq!(t["tokens"]["completion_tokens"], 40);
    assert_eq!(t["tokens"]["reasoning_tokens"], 5);
    assert_eq!(t["tokens"]["cached_prompt_tokens"], 100);
    assert_eq!(t["tokens"]["cache_read_tokens"], 50);
    assert_eq!(t["tokens"]["cache_creation_tokens"], 50);
    assert_eq!(t["tokens"]["input_tokens"], 400);
    assert_eq!(t["tokens"]["cache_hit_tokens"], 150);
    near(&t["tokens"]["cache_hit_rate"], 0.375);
    assert!(t["charged_micro_cny"].as_str().is_some());
    assert_eq!(t["response_cache"]["hit_requests"], 1);
    assert_eq!(t["response_cache"]["miss_requests"], 1);
    assert_eq!(t["response_cache"]["disabled_requests"], 1);
    near(&t["response_cache"]["hit_rate"], 0.5);
    assert!(!r.body.to_string().contains("stream_aborted"));
}

#[tokio::test]
async fn scopes_use_key_ownership_and_final_counts_are_not_a_page_or_client_id_dedup() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let member = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let key = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let outside = seed_tenant(&w.cp, &w.admin).await;
    let outside_key = seed_key(&w.cp, &w.admin, outside, None).await;
    let mut rows = Vec::new();
    for n in 0..1101 {
        let mut e = event(&w, &format!("full-{n}"), model, json!({"prompt_tokens": 1}));
        e["request_id"] = json!("client-reuses-id");
        rows.push(e);
    }
    for (id, k) in [("member", &key), ("foreign", &outside_key)] {
        let mut e = event(&w, id, model, json!({}));
        e["api_key_id"] = json!(k);
        e["user_id"] = json!(w.member);
        e["team_id"] = json!(w.tenant);
        rows.push(e);
    }
    intake(&w, rows).await;
    let r =
        w.cp.send("GET", "/api/usage/metrics", Some(owner.cookie()), None)
            .await;
    assert_eq!(totals(&r)["requests"], 1102);
    let r =
        w.cp.send("GET", "/api/usage/metrics", Some(member.cookie()), None)
            .await;
    assert_eq!(totals(&r)["requests"], 1);
    let r =
        w.cp.send(
            "GET",
            &format!("/api/usage/metrics?member_id={}", w.member),
            Some(member.cookie()),
            None,
        )
        .await;
    assert_eq!(totals(&r)["requests"], 0, "member filter不能扩大自身范围");
    for filter in [
        format!("member_id={}", member.id),
        format!("api_key_id={key}"),
    ] {
        let r =
            w.cp.send(
                "GET",
                &format!("/api/usage/metrics?{filter}"),
                Some(owner.cookie()),
                None,
            )
            .await;
        assert_eq!(totals(&r)["requests"], 1);
    }
    let r =
        w.cp.send(
            "GET",
            &format!("/admin/usage/metrics?tenant_id={}", w.tenant),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(totals(&r)["requests"], 1102);
    let r =
        w.cp.send("GET", "/admin/usage/metrics", Some(w.admin.cookie()), None)
            .await;
    assert_eq!(totals(&r)["requests"], 1103);
}

#[tokio::test]
async fn empty_unknown_old_fields_and_time_bucket_bounds_remain_honest() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let r =
        w.cp.send("GET", "/api/usage/metrics", Some(owner.cookie()), None)
            .await;
    let t = totals(&r);
    assert_eq!(t["requests"], 0);
    assert!(t["success_rate"].is_null());
    assert_eq!(t["first_token_latency_ms"]["sample_count"], 0);
    assert!(t["first_token_latency_ms"]["avg"].is_null());
    assert!(t["tokens"]["cache_hit_rate"].is_null());
    assert!(t["response_cache"]["hit_rate"].is_null());
    let mut old = event(&w, "old", model, json!({"prompt_tokens": 1}));
    old.as_object_mut()
        .expect("event")
        .remove("is_final_attempt");
    let mut unknown = event(&w, "unknown", model, json!({}));
    for k in ["prompt_tokens", "completion_tokens", "status_code"] {
        unknown.as_object_mut().expect("event").remove(k);
    }
    intake(&w, vec![old, unknown]).await;
    let r =
        w.cp.send("GET", "/api/usage/metrics", Some(owner.cookie()), None)
            .await;
    let t = totals(&r);
    assert_eq!(t["requests"], 1);
    assert_eq!(t["unknown_final_attempts"], 1);
    assert_eq!(t["unknown_outcome_requests"], 1);
    assert_eq!(t["known_outcome_requests"], 0);
    assert!(t["success_rate"].is_null());
    assert_eq!(t["unpriced_attempts"], 0);
    assert_eq!(t["tokens"]["unknown_usage_attempts"], 1);
    assert_eq!(t["first_token_latency_ms"]["missing_count"], 1);
    let from = Utc::now() - TimeDelta::days(100);
    let to = from + TimeDelta::days(93);
    let uri = format!(
        "/api/usage/metrics?from={}&to={}",
        from.to_rfc3339_opts(SecondsFormat::Nanos, true),
        to.to_rfc3339_opts(SecondsFormat::Nanos, true)
    );
    let r = w.cp.send("GET", &uri, Some(owner.cookie()), None).await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(
        r.body["details"][0]["path"], "/to",
        "真正验证范围上限而非日期解析"
    );
    let r =
        w.cp.send(
            "GET",
            "/api/usage/metrics?bucket=minute",
            Some(owner.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r =
        w.cp.send(
            "GET",
            "/api/usage/metrics?bucket=second",
            Some(owner.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
}

#[tokio::test]
async fn utc_buckets_boundaries_model_privacy_and_raw_percentiles_are_complete() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let hidden = seed_model(&w.cp, &w.admin).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let r =
        w.cp.send(
            "GET",
            &format!("/admin/tenants/{}", w.tenant),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let version = r.body["version"].as_i64().expect("version");
    let r =
        w.cp.send_conditional(
            "PATCH",
            &format!("/admin/tenants/{}", w.tenant),
            w.admin.cookie(),
            version,
            json!({"allowed_model_ids": [model]}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let from = DateTime::from_timestamp(Utc::now().timestamp().div_euclid(3600) * 3600, 0)
        .expect("time")
        - TimeDelta::hours(3);
    let to = from + TimeDelta::hours(3);
    let mut rows = Vec::new();
    for (id, at, sampled, selected) in [
        ("at-from", from, 0, model),
        ("same-hour", from + TimeDelta::minutes(10), 100, model),
        ("third-hour", from + TimeDelta::hours(2), 1000, hidden),
        ("at-to", to, 9000, model),
        ("before-from", from - TimeDelta::nanoseconds(1), 9000, model),
    ] {
        let mut e = event(&w, id, selected, json!({"first_token_latency_ms": sampled}));
        e["occurred_at"] = json!(at.to_rfc3339_opts(SecondsFormat::Nanos, true));
        rows.push(e);
    }
    intake(&w, rows).await;
    let range = format!(
        "from={}&to={}&bucket=hour",
        from.to_rfc3339_opts(SecondsFormat::Nanos, true),
        to.to_rfc3339_opts(SecondsFormat::Nanos, true)
    );
    let r =
        w.cp.send(
            "GET",
            &format!("/api/usage/metrics?{range}"),
            Some(owner.cookie()),
            None,
        )
        .await;
    let t = totals(&r);
    assert_eq!(t["requests"], 3);
    near(&t["first_token_latency_ms"]["p50"], 100.0);
    near(&t["first_token_latency_ms"]["p95"], 910.0);
    assert_eq!(
        t["first_token_latency_ms"]["sample_count"], 3,
        "实测零值也应保留"
    );
    let series = r.body["series"].as_array().expect("series");
    assert_eq!(series.len(), 3);
    assert_eq!(series[0]["requests"], 2);
    assert_eq!(series[1]["requests"], 0);
    assert!(series[1]["first_token_latency_ms"]["p95"].is_null());
    assert_eq!(series[2]["requests"], 1);
    assert_eq!(
        series[0]["period_start"],
        from.to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let models = r.body["models"].as_array().expect("models");
    assert_eq!(models.len(), 2);
    assert!(models
        .iter()
        .any(|v| v["model"]["id"] == model.to_string() && v["requests"] == 2));
    assert!(models
        .iter()
        .any(|v| v["model"]["id"].is_null() && v["model"]["name"] == "" && v["requests"] == 1));
    assert!(!r.body.to_string().contains(&hidden.to_string()));
    let r =
        w.cp.send(
            "GET",
            &format!("/api/usage/metrics?{range}&model_id={model}"),
            Some(owner.cookie()),
            None,
        )
        .await;
    assert_eq!(totals(&r)["requests"], 2);
    let r =
        w.cp.send(
            "GET",
            &format!("/admin/tenants/{}/usage/metrics?{range}", w.tenant),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(totals(&r)["requests"], 3);
    let too_many = format!(
        "/api/usage/metrics?from={}&to={}&bucket=minute",
        from.to_rfc3339_opts(SecondsFormat::Nanos, true),
        (from + TimeDelta::days(2)).to_rfc3339_opts(SecondsFormat::Nanos, true)
    );
    let r =
        w.cp.send("GET", &too_many, Some(owner.cookie()), None)
            .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/bucket");
    let too_long = format!(
        "/api/usage/metrics?from={}&to={}",
        from.to_rfc3339_opts(SecondsFormat::Nanos, true),
        (from + TimeDelta::days(93)).to_rfc3339_opts(SecondsFormat::Nanos, true)
    );
    let r =
        w.cp.send("GET", &too_long, Some(owner.cookie()), None)
            .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/to");
}

#[tokio::test]
async fn repricing_is_exact_above_javascript_integer_range_and_never_multiplies_events() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    intake(
        &w,
        vec![
            event(&w, "money-1", model, json!({"prompt_tokens": 1})),
            event(&w, "money-2", model, json!({"prompt_tokens": 1})),
        ],
    )
    .await;
    for delta in [9_007_199_254_740_993_i64, 7] {
        let run = Uuid::new_v4();
        sqlx::query("INSERT INTO billing_repricing_runs_cny \
            (id, status, filter, reason, pricing_version, engine_version, events, changed, \
             still_unpriced, skipped_unattributed, delta_micro_cny, by_tenant, created_by, applied_by, applied_at) \
            VALUES ($1, 'applied', '{}', '精度覆盖', 0, $2, 1, 1, 0, 0, $3, $4, $5, $5, now())")
            .bind(run).bind(i64::from(aisix_pricing::ENGINE_VERSION)).bind(delta)
            .bind(json!([{"tenant_id": w.tenant, "events": 1, "changed": 1, "delta_micro_cny": delta}]))
            .bind(w.admin.id).execute(w.cp.db().pool()).await.expect("run");
        sqlx::query("INSERT INTO pricing_adjustments_cny (run_id, event_id, delta_micro_cny, engine_version, reason, created_by) \
            VALUES ($1, 'money-1', $2, $3, '精度覆盖', $4)")
            .bind(run).bind(delta).bind(i64::from(aisix_pricing::ENGINE_VERSION)).bind(w.admin.id)
            .execute(w.cp.db().pool()).await.expect("adjustment");
    }
    let r =
        w.cp.send("GET", "/api/usage/metrics", Some(owner.cookie()), None)
            .await;
    let t = totals(&r);
    assert_eq!(t["requests"], 2);
    assert_eq!(t["billing_attempts"], 2);
    assert_eq!(t["charged_micro_cny"], "9007199254741002");
    assert_eq!(t["adjusted_micro_cny"], "9007199254741000");
    assert_eq!(t["tokens"]["prompt_tokens"], 2);
    assert_eq!(t["unpriced_attempts"], 0);
}

#[tokio::test]
async fn positive_unpriced_usage_and_unknown_outcomes_have_explicit_coverage() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let pending = event(&w, "positive-pending", model, json!({"prompt_tokens": 100}));
    let mut unknown = event(&w, "unknown-outcome", model, json!({}));
    unknown
        .as_object_mut()
        .expect("event")
        .remove("status_code");
    intake(&w, vec![pending, unknown]).await;
    let r =
        w.cp.send("GET", "/api/usage/metrics", Some(owner.cookie()), None)
            .await;
    let t = totals(&r);
    assert_eq!(t["requests"], 2);
    assert_eq!(t["succeeded_requests"], 1);
    assert_eq!(t["known_outcome_requests"], 1);
    assert_eq!(t["unknown_outcome_requests"], 1);
    near(&t["success_rate"], 1.0);
    assert_eq!(t["unpriced_attempts"], 1, "正token且无价格不能当免费");
    assert_eq!(t["charged_micro_cny"], "0");
}

#[tokio::test]
async fn malformed_or_inconsistent_cache_usage_is_unknown_instead_of_zero_or_over_one() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    intake(&w, vec![
        event(&w, "too-much-cache", model, json!({"prompt_tokens": 100, "completion_tokens": 1, "cached_prompt_tokens": 200})),
        event(&w, "bad-cache-field", model, json!({"prompt_tokens": 100, "completion_tokens": 1, "cache_read_tokens": "broken"})),
    ]).await;
    let r =
        w.cp.send("GET", "/api/usage/metrics", Some(owner.cookie()), None)
            .await;
    let t = totals(&r);
    assert_eq!(t["requests"], 2);
    assert_eq!(t["tokens"]["known_usage_attempts"], 0);
    assert_eq!(t["tokens"]["unknown_usage_attempts"], 2);
    assert!(t["tokens"]["cache_hit_rate"].is_null());
    assert_eq!(t["tokens"]["input_tokens"], 0);
    intake(
        &w,
        vec![event(
            &w,
            "nullable-cache-write",
            model,
            json!({"prompt_tokens": 100,
        "completion_tokens": 1, "cache_write_tokens": null}),
        )],
    )
    .await;
    let r =
        w.cp.send("GET", "/api/usage/metrics", Some(owner.cookie()), None)
            .await;
    let t = totals(&r);
    assert_eq!(t["tokens"]["known_usage_attempts"], 1);
    assert_eq!(t["tokens"]["unknown_usage_attempts"], 2);
    assert_eq!(t["tokens"]["input_tokens"], 100);
}
