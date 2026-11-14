//! 直连监测从真实遥测入口写入独立数据库；失败尝试不能受收费可见性过滤。
mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use chrono::{SecondsFormat, TimeDelta, Utc};
use serde_json::{json, Value};
use support::world::{deliver, seed_model, usage, world, World};
use support::Resp;
use uuid::Uuid;

const ENDPOINT: &str = "/admin/usage/direct-models";

async fn provider(w: &World, model: Uuid) -> Uuid {
    let value: String = sqlx::query_scalar(
        "SELECT doc->>'provider_key_id' FROM gw_resources WHERE kind = 'models' AND id = $1",
    )
    .bind(model)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("模型所属账号");
    Uuid::parse_str(&value).expect("账号 ID")
}

async fn intake(w: &World, rows: Vec<Value>) {
    let response = deliver(&w.dp, Uuid::new_v4(), rows).await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.body["rejected"], 0, "{response:?}");
}

fn near(value: &Value, expected: f64) {
    let actual = value
        .as_f64()
        .unwrap_or_else(|| panic!("缺少实测值: {value}"));
    assert!(
        (actual - expected).abs() < 0.000001,
        "{actual} != {expected}"
    );
}

fn model(response: &Resp, id: Uuid) -> &Value {
    assert_eq!(response.status, 200, "{response:?}");
    response.body["models"]
        .as_array()
        .expect("模型清单")
        .iter()
        .find(|row| row["model"]["id"] == json!(id))
        .expect("真实直连模型")
}

#[tokio::test]
async fn direct_attempts_keep_failed_retries_cache_shapes_and_measured_throughput() {
    let Some(w) = world().await else { return };
    let first = seed_model(&w.cp, &w.admin).await;
    let second = seed_model(&w.cp, &w.admin).await;
    let idle = seed_model(&w.cp, &w.admin).await;
    let first_pk = provider(&w, first).await;
    let second_pk = provider(&w, second).await;
    // 入口模型与实际目标不同，监测必须按后者归属。
    let parent = Uuid::new_v4();
    let mut rows = Vec::new();
    for (id, target, account, fields) in [
        (
            "monitor-a1",
            first,
            first_pk,
            json!({"prompt_tokens":100,"completion_tokens":31,"cached_prompt_tokens":80,"upstream_latency_ms":400,"upstream_ttft_ms":100,"first_token_latency_ms":0,"upstream_tpot_ms":10}),
        ),
        (
            "monitor-a2",
            first,
            first_pk,
            json!({"prompt_tokens":50,"completion_tokens":21,"cache_read_tokens":25,"cache_creation_tokens":25,"upstream_latency_ms":600,"upstream_ttft_ms":200,"first_token_latency_ms":300,"upstream_tpot_ms":20}),
        ),
        (
            "monitor-retry",
            first,
            first_pk,
            json!({"status_code":503,"error_class":"upstream_status","upstream_latency_ms":200,"is_final_attempt":false}),
        ),
        (
            "monitor-cache",
            first,
            first_pk,
            json!({"prompt_tokens":100,"cached_prompt_tokens":100,"cache_status":"hit"}),
        ),
        (
            "monitor-local",
            first,
            first_pk,
            json!({"status_code":400,"error_class":"invalid_request_error"}),
        ),
        (
            "monitor-unknown",
            first,
            first_pk,
            json!({"status_code":null,"upstream_latency_ms":500}),
        ),
        (
            "monitor-b",
            second,
            second_pk,
            json!({"prompt_tokens":20,"completion_tokens":11,"upstream_latency_ms":150,"upstream_ttft_ms":100,"first_token_latency_ms":120,"upstream_tpot_ms":5}),
        ),
    ] {
        let mut event = usage(&w, id, parent, fields);
        event["model_id"] = json!(target);
        event["provider_key_id"] = json!(account);
        rows.push(event);
    }
    intake(&w, rows).await;
    let response =
        w.cp.send("GET", ENDPOINT, Some(w.admin.cookie()), None)
            .await;
    let first_row = model(&response, first);
    assert_eq!(first_row["provider_key"]["id"], json!(first_pk));
    assert_eq!(first_row["attributed_attempts"], 6);
    assert_eq!(first_row["attempts"], 4);
    assert_eq!(first_row["excluded_attempts"], 2);
    assert_eq!(first_row["succeeded_attempts"], 2);
    assert_eq!(first_row["failed_attempts"], 1);
    assert_eq!(first_row["unknown_outcome_attempts"], 1);
    near(&first_row["failure_rate"], 1.0 / 3.0);
    near(&first_row["success_rate"], 2.0 / 3.0);
    assert_eq!(first_row["upstream_ttft_ms"]["sample_count"], 2);
    assert_eq!(first_row["upstream_ttft_ms"]["missing_count"], 2);
    near(&first_row["upstream_ttft_ms"]["p95"], 195.0);
    assert_eq!(first_row["first_token_latency_ms"]["sample_count"], 2);
    near(&first_row["first_token_latency_ms"]["p50"], 150.0);
    assert_eq!(first_row["tokens"]["input_tokens"], 200);
    assert_eq!(first_row["tokens"]["cache_hit_tokens"], 105);
    near(&first_row["tokens"]["cache_hit_rate"], 0.525);
    assert_eq!(first_row["throughput_tps"]["sample_count"], 2);
    assert_eq!(first_row["throughput_tps"]["missing_count"], 2);
    near(&first_row["throughput_tps"]["avg"], 1000.0 * 50.0 / 700.0);
    near(&first_row["throughput_tps"]["p50"], 75.0);
    near(&model(&response, second)["throughput_tps"]["avg"], 200.0);
    let no_traffic = model(&response, idle);
    assert_eq!(no_traffic["attempts"], 0);
    assert!(no_traffic
        .get("failure_rate")
        .expect("空样本率字段")
        .is_null());
    assert!(no_traffic["throughput_tps"]
        .get("avg")
        .expect("空吞吐字段")
        .is_null());
    assert_eq!(response.body["totals"]["attempts"], 5);
    assert_eq!(response.body["totals"]["failed_attempts"], 1);
    assert_eq!(response.body["totals"]["succeeded_attempts"], 3);
    assert_eq!(response.body["totals"]["unknown_outcome_attempts"], 1);
    assert!(!response.body.to_string().contains("sk-pricing-upstream"));
}

#[tokio::test]
async fn direct_monitoring_filters_before_aggregation_and_is_operator_only() {
    let Some(w) = world().await else { return };
    let target = seed_model(&w.cp, &w.admin).await;
    let pk = provider(&w, target).await;
    let from = Utc::now() - TimeDelta::hours(2);
    let mut rows = Vec::new();
    for (id, tier, status, offset) in [
        ("mon-filter-a", "fast", 200, 10),
        ("mon-filter-b", "fast", 500, 20),
        ("mon-filter-c", "default", 500, 30),
    ] {
        let mut row = usage(
            &w,
            id,
            target,
            json!({"model_id":target,"provider_key_id":pk,"service_tier":tier,"status_code":status,"upstream_latency_ms":100,"is_final_attempt":true}),
        );
        row["occurred_at"] = json!((from + TimeDelta::minutes(offset)).to_rfc3339());
        rows.push(row);
    }
    intake(&w, rows).await;
    let query = format!("{ENDPOINT}?from={}&to={}&bucket=hour&model_id={target}&provider_key_id={pk}&service_tier=fast&outcome=failed", from.to_rfc3339_opts(SecondsFormat::Secs,true), (from+TimeDelta::hours(2)).to_rfc3339_opts(SecondsFormat::Secs,true));
    let response = w.cp.send("GET", &query, Some(w.admin.cookie()), None).await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.body["models"].as_array().expect("筛选模型").len(),
        1
    );
    assert_eq!(response.body["totals"]["attempts"], 1);
    assert_eq!(response.body["totals"]["failed_attempts"], 1);
    near(&response.body["totals"]["failure_rate"], 1.0);
    assert_eq!(
        response.body["series"]
            .as_array()
            .expect("全部时间桶")
            .iter()
            .map(|r| r["attempts"].as_i64().expect("桶计数"))
            .sum::<i64>(),
        1
    );
    let member = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let denied =
        w.cp.send("GET", ENDPOINT, Some(member.cookie()), None)
            .await;
    assert_eq!(denied.status, 401, "租户会话不能读取上游监测: {denied:?}");
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let allowed =
        w.cp.send("GET", ENDPOINT, Some(viewer.cookie()), None)
            .await;
    assert_eq!(allowed.status, 200, "运维只读角色可监测: {allowed:?}");
}

#[tokio::test]
async fn direct_monitoring_keeps_malformed_and_missing_measurements_unknown() {
    let Some(w) = world().await else { return };
    let target = seed_model(&w.cp, &w.admin).await;
    let pk = provider(&w, target).await;
    let mut rows = Vec::new();
    for (id, extra) in [
        ("mon-bad-tpot", json!({"upstream_tpot_ms":-5})),
        ("mon-zero-tpot", json!({"upstream_tpot_ms":0})),
        ("mon-text-tpot", json!({"upstream_tpot_ms":"10"})),
        ("mon-missing-tpot", json!({})),
    ] {
        let mut row = usage(
            &w,
            id,
            target,
            json!({"model_id":target,"provider_key_id":pk,"completion_tokens":11,"upstream_latency_ms":100}),
        );
        for (key, value) in extra.as_object().expect("附加量") {
            row[key] = value.clone();
        }
        rows.push(row);
    }
    intake(&w, rows).await;
    let response =
        w.cp.send("GET", ENDPOINT, Some(w.admin.cookie()), None)
            .await;
    let measured = model(&response, target);
    assert_eq!(measured["attempts"], 4);
    assert_eq!(measured["throughput_tps"]["sample_count"], 0);
    assert_eq!(measured["throughput_tps"]["missing_count"], 4);
    assert!(measured["throughput_tps"]
        .get("avg")
        .expect("明确未知")
        .is_null());
    assert_eq!(measured["upstream_ttft_ms"]["missing_count"], 4);
    let invalid =
        w.cp.send(
            "GET",
            &format!("{ENDPOINT}?bucket=minute&from=2026-01-01T00:00:00Z&to=2026-01-03T00:00:00Z"),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(invalid.status, 422, "时间桶界限: {invalid:?}");
}

#[tokio::test]
async fn direct_monitoring_tenant_scope_keeps_unpriced_retries_and_uses_key_ownership() {
    let Some(w) = world().await else { return };
    let target = seed_model(&w.cp, &w.admin).await;
    let pk = provider(&w, target).await;
    let retry = usage(
        &w,
        "mon-owned-retry",
        target,
        json!({"model_id":target,"provider_key_id":pk,"status_code":503,
            "error_class":"upstream_status","upstream_latency_ms":10,
            "is_final_attempt":false,"team_id":Uuid::new_v4()}),
    );
    let success = usage(
        &w,
        "mon-owned-final",
        target,
        json!({"model_id":target,"provider_key_id":pk,"upstream_latency_ms":20,
            "is_final_attempt":true}),
    );
    let unrelated = usage(
        &w,
        "mon-unowned-key",
        target,
        json!({"model_id":target,"provider_key_id":pk,"upstream_latency_ms":30,
            "api_key_id":Uuid::new_v4()}),
    );
    intake(&w, vec![retry, success, unrelated]).await;
    let query = format!("{ENDPOINT}?tenant_id={}&model_id={target}", w.tenant);
    let response = w.cp.send("GET", &query, Some(w.admin.cookie()), None).await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.body["totals"]["attempts"], 2);
    assert_eq!(response.body["totals"]["failed_attempts"], 1);
    assert_eq!(response.body["totals"]["succeeded_attempts"], 1);
    near(&response.body["totals"]["failure_rate"], 0.5);
    let retry_status: String = sqlx::query_scalar(
        "SELECT status FROM pricing_charges_cny WHERE event_id = 'mon-owned-retry'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("失败重试也保存归属");
    assert_eq!(retry_status, "unpriced", "未收费重试仍属于监测样本");
}

#[tokio::test]
async fn direct_monitoring_does_not_blame_gateway_failures_on_the_upstream() {
    let Some(w) = world().await else { return };
    let target = seed_model(&w.cp, &w.admin).await;
    let pk = provider(&w, target).await;
    let mut rows = Vec::new();
    for (id, status, class) in [
        ("mon-informational", json!(102), ""),
        ("mon-redirect", json!(301), ""),
        ("mon-http-failure", json!(599), ""),
        ("mon-invalid-status", json!(600), ""),
        ("mon-text-status", json!("200"), ""),
        ("mon-large-status", json!(1e100), ""),
        ("mon-output-blocked", json!(400), "content_filter"),
        (
            "mon-local-unavailable",
            json!(503),
            "dependency_unavailable",
        ),
        ("mon-client-left", json!(499), "client_disconnected"),
        ("mon-upstream-failure", json!(503), "upstream_status"),
    ] {
        rows.push(usage(
            &w,
            id,
            target,
            json!({"model_id":target,"provider_key_id":pk,"upstream_latency_ms":10,
                "status_code":status,"error_class":class}),
        ));
    }
    intake(&w, rows).await;
    let response =
        w.cp.send("GET", ENDPOINT, Some(w.admin.cookie()), None)
            .await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.body["totals"]["attempts"], 10);
    assert_eq!(response.body["totals"]["failed_attempts"], 3);
    assert_eq!(response.body["totals"]["succeeded_attempts"], 0);
    assert_eq!(response.body["totals"]["unknown_outcome_attempts"], 7);
    for (outcome, expected) in [("failed", 3), ("unknown", 7)] {
        let response =
            w.cp.send(
                "GET",
                &format!("{ENDPOINT}?model_id={target}&outcome={outcome}"),
                Some(w.admin.cookie()),
                None,
            )
            .await;
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(
            response.body["totals"]["attempts"], expected,
            "{response:?}"
        );
    }
}

#[tokio::test]
async fn direct_monitoring_respects_explicit_dispatch_without_guessing_missing_values() {
    let Some(w) = world().await else { return };
    let target = seed_model(&w.cp, &w.admin).await;
    let pk = provider(&w, target).await;
    let mut rows = Vec::new();
    for (id, extra) in [
        ("mon-submillisecond", json!({"upstream_dispatched":true})),
        (
            "mon-not-dispatched",
            json!({"upstream_dispatched":false,"upstream_latency_ms":100}),
        ),
        (
            "mon-cache-wins",
            json!({"upstream_dispatched":true,"cache_status":"hit"}),
        ),
        ("mon-string-dispatch", json!({"upstream_dispatched":"true"})),
        ("mon-unknown-dispatch", json!({})),
    ] {
        let mut row = usage(
            &w,
            id,
            target,
            json!({"model_id":target,"provider_key_id":pk,"upstream_latency_ms":0}),
        );
        for (key, value) in extra.as_object().expect("附加派发事实") {
            row[key] = value.clone();
        }
        rows.push(row);
    }
    intake(&w, rows).await;
    let response =
        w.cp.send("GET", ENDPOINT, Some(w.admin.cookie()), None)
            .await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.body["totals"]["attributed_attempts"], 5);
    assert_eq!(response.body["totals"]["attempts"], 1);
    assert_eq!(response.body["totals"]["excluded_attempts"], 2);
    assert_eq!(response.body["totals"]["unmeasured_attempts"], 2);
    assert_eq!(response.body["totals"]["succeeded_attempts"], 1);
    assert_eq!(
        response.body["totals"]["attempt_duration_ms"]["sample_count"],
        0
    );
    assert_eq!(
        response.body["totals"]["attempt_duration_ms"]["missing_count"],
        1
    );
}

#[tokio::test]
async fn direct_monitoring_cache_rate_uses_reported_samples_and_keeps_zero_distinct() {
    let Some(w) = world().await else { return };
    let missing = seed_model(&w.cp, &w.admin).await;
    let zero = seed_model(&w.cp, &w.admin).await;
    let target = seed_model(&w.cp, &w.admin).await;
    let missing_pk = provider(&w, missing).await;
    let zero_pk = provider(&w, zero).await;
    let pk = provider(&w, target).await;
    let mut rows = vec![
        usage(
            &w,
            "mon-cache-unreported-only",
            missing,
            json!({"model_id":missing,"provider_key_id":missing_pk,"prompt_tokens":100,
                "upstream_latency_ms":10}),
        ),
        usage(
            &w,
            "mon-cache-zero-only",
            zero,
            json!({"model_id":zero,"provider_key_id":zero_pk,"prompt_tokens":100,
                "upstream_latency_ms":10,"cached_prompt_tokens":0}),
        ),
    ];
    for (id, extra) in [
        ("mon-cache-unreported", json!({"prompt_tokens":100})),
        (
            "mon-cache-measured-zero",
            json!({"prompt_tokens":100,"cached_prompt_tokens":0}),
        ),
        (
            "mon-cache-measured-hit",
            json!({"prompt_tokens":100,"cached_prompt_tokens":50}),
        ),
    ] {
        let mut row = usage(
            &w,
            id,
            target,
            json!({"model_id":target,"provider_key_id":pk,"upstream_latency_ms":10}),
        );
        for (key, value) in extra.as_object().expect("缓存样本") {
            row[key] = value.clone();
        }
        rows.push(row);
    }
    intake(&w, rows).await;
    let response =
        w.cp.send("GET", ENDPOINT, Some(w.admin.cookie()), None)
            .await;
    let tokens = &model(&response, target)["tokens"];
    assert_eq!(tokens["input_tokens"], 300);
    assert_eq!(tokens["cache_hit_tokens"], 50);
    assert_eq!(tokens["cache_measured_attempts"], 2);
    assert_eq!(tokens["cache_missing_attempts"], 1);
    assert_eq!(tokens["cache_measured_input_tokens"], 200);
    assert_eq!(tokens["cache_measured_hit_tokens"], 50);
    near(&tokens["cache_hit_rate"], 0.25);
    let missing_tokens = &model(&response, missing)["tokens"];
    assert_eq!(missing_tokens["known_usage_attempts"], 1);
    assert_eq!(missing_tokens["cache_measured_attempts"], 0);
    assert_eq!(missing_tokens["cache_missing_attempts"], 1);
    assert!(missing_tokens
        .get("cache_hit_rate")
        .expect("明确未知缓存率")
        .is_null());
    let zero_tokens = &model(&response, zero)["tokens"];
    assert_eq!(zero_tokens["cache_measured_attempts"], 1);
    near(&zero_tokens["cache_hit_rate"], 0.0);
}

#[tokio::test]
async fn serialized_telemetry_keeps_reported_zero_in_cache_denominator() {
    let Some(w) = world().await else { return };
    for additive in [false, true] {
        let target = seed_model(&w.cp, &w.admin).await;
        let pk = provider(&w, target).await;
        let mut rows = Vec::new();
        for (index, reported, hit) in [(0, Some(true), 80), (1, Some(true), 0), (2, Some(false), 0)]
        {
            let stats = if additive {
                let mut usage =
                    json!({"input_tokens": if index == 0 { 20 } else { 100 }, "output_tokens":0});
                if reported == Some(true) {
                    usage["cache_read_input_tokens"] = json!(hit);
                    usage["cache_creation_input_tokens"] = json!(0);
                }
                aisix_provider_anthropic::wire::anthropic_usage_stats(&usage)
            } else {
                let mut usage = json!({"prompt_tokens":100,"completion_tokens":0});
                if reported == Some(true) {
                    usage["prompt_tokens_details"] = json!({"cached_tokens":hit});
                }
                aisix_provider_openai::wire::into_usage(
                    serde_json::from_value(usage).expect("真实上游用量格式"),
                )
            };
            let event = aisix_obs::UsageEvent {
                event_id: Uuid::new_v4().to_string(),
                request_id: Uuid::new_v4().to_string(),
                occurred_at: Utc::now().to_rfc3339(),
                model_id: target.to_string(),
                requested_model_id: target.to_string(),
                provider_key_id: Some(pk.to_string()),
                api_key_id: w.key.clone(),
                team_id: w.tenant.to_string(),
                user_id: w.member.to_string(),
                operation: "chat".into(),
                prompt_tokens: stats.prompt_tokens,
                cached_prompt_tokens: stats.cached_prompt_tokens,
                cache_read_tokens: stats.cache_read_tokens,
                cache_creation_tokens: stats.cache_creation_tokens,
                cache_usage_reported: stats.cache_usage_reported(),
                status_code: 200,
                upstream_dispatched: Some(true),
                is_final_attempt: true,
                ..Default::default()
            };
            let wire = serde_json::to_value(event).expect("序列化真实用量事件");
            if index > 0 {
                assert!(wire.get("cached_prompt_tokens").is_none());
                assert!(wire.get("cache_read_tokens").is_none());
                assert!(wire.get("cache_creation_tokens").is_none());
            }
            rows.push(wire);
        }
        intake(&w, rows).await;
        let response =
            w.cp.send("GET", ENDPOINT, Some(w.admin.cookie()), None)
                .await;
        let tokens = &model(&response, target)["tokens"];
        assert_eq!(tokens["cache_measured_attempts"], 2);
        assert_eq!(tokens["cache_missing_attempts"], 1);
        assert_eq!(tokens["cache_measured_input_tokens"], 200);
        assert_eq!(tokens["cache_measured_hit_tokens"], 80);
        near(&tokens["cache_hit_rate"], 0.4);
    }
}
