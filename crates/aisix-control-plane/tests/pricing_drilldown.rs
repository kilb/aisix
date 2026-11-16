//! 状态页的明细链接应保留同一集合；这里以真实 PG 和 HTTP handler 核对事件编号。
mod support;

use std::collections::BTreeSet;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use support::world::{db_now, group_id, seed_model, world, World};
use uuid::Uuid;

// 查询夹具只声明历史状态与身份，不伪造 token 计量；不测试计价引擎本身。
type HistoricalRow<'a> = (
    &'a str,
    &'a str,
    Option<Uuid>,
    &'a str,
    Option<&'a str>,
    bool,
    DateTime<Utc>,
    DateTime<Utc>,
);

async fn row(w: &World, fields: HistoricalRow<'_>) {
    let (id, model, group, code, tier, batch, occurred, priced) = fields;
    let mut event = json!({"event_id": id, "requested_model_id": model, "is_batch": batch});
    if let Some(tier) = tier {
        event["service_tier"] = json!(tier);
    }
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, event) \
         VALUES ($1, $2, $2, $3, $4)",
    )
    .bind(id)
    .bind(Uuid::new_v4())
    .bind(occurred)
    .bind(event)
    .execute(w.cp.db().pool())
    .await
    .expect("历史用量身份行");
    sqlx::query(
        "INSERT INTO pricing_charges_cny \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, \
          status, group_id, error_code, engine_version, priced_at) \
         VALUES ($1, $2, $3, $4, $5, $6, 'unpriced', $7, $8, 2, $9)",
    )
    .bind(id)
    .bind(w.tenant.to_string())
    .bind(&w.key)
    .bind(w.member.to_string())
    .bind(model)
    .bind(occurred)
    .bind(group)
    .bind(code)
    .bind(priced)
    .execute(w.cp.db().pool())
    .await
    .expect("历史未计价状态行");
}

async fn corrected(w: &World, event: &str) {
    let run = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO billing_repricing_runs_cny \
         (id, status, filter, reason, pricing_version, engine_version, events, changed, \
          still_unpriced, skipped_unattributed, delta_micro_cny, by_tenant, created_by, \
          applied_by, applied_at) \
         VALUES ($1, 'applied', '{}', '历史更正', 0, 2, 1, 1, 0, 0, 1, '[]', $2, $2, now())",
    )
    .bind(run)
    .bind(w.admin.id)
    .execute(w.cp.db().pool())
    .await
    .expect("已应用更正");
    sqlx::query(
        "INSERT INTO pricing_adjustments_cny \
         (run_id, event_id, delta_micro_cny, engine_version, reason, created_by) \
         VALUES ($1, $2, 1, 2, '历史更正', $3)",
    )
    .bind(run)
    .bind(event)
    .bind(w.admin.id)
    .execute(w.cp.db().pool())
    .await
    .expect("更正行");
}

async fn get(w: &World, query: &[(String, String)]) -> Value {
    let mut url = reqwest::Url::parse(&w.cp.url("/admin/pricing/charges")).expect("本地 API");
    url.query_pairs_mut()
        .extend_pairs(query.iter().map(|(k, v)| (k, v)));
    let path = format!("{}?{}", url.path(), url.query().expect("查询条件"));
    let r = w.cp.send("GET", &path, Some(w.admin.cookie()), None).await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body
}

fn ids(answer: &Value) -> BTreeSet<String> {
    answer["items"]
        .as_array()
        .expect("明细数组")
        .iter()
        .map(|r| r["event_id"].as_str().expect("显式事件编号").to_string())
        .collect()
}

#[tokio::test]
async fn status_drilldown_keeps_the_exact_group_tier_pricing_window_and_uncorrected_set() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await.to_string();
    let other = seed_model(&w.cp, &w.admin).await.to_string();
    let a = group_id(&w.cp, "shared").await;
    let b = support::seed_group(&w.cp, &w.admin, "drilldown-b").await;
    let at = db_now(&w.cp).await;
    let recent = at - TimeDelta::minutes(1);
    for (id, m, group, code, tier, batch, occurred, priced) in [
        (
            "priority",
            &*model,
            Some(a),
            "missing_tier_price",
            Some("priority"),
            false,
            recent,
            recent,
        ),
        (
            "late",
            &*model,
            Some(a),
            "missing_tier_price",
            Some("priority"),
            false,
            at - TimeDelta::days(3),
            recent,
        ),
        (
            "fast",
            &*model,
            Some(a),
            "missing_tier_price",
            Some("fast"),
            false,
            recent,
            recent,
        ),
        (
            "missing",
            &*model,
            Some(a),
            "missing_tier_price",
            None,
            false,
            recent,
            recent,
        ),
        (
            "empty",
            &*model,
            Some(a),
            "missing_tier_price",
            Some(""),
            false,
            recent,
            recent,
        ),
        (
            "batch-priority",
            &*model,
            Some(a),
            "missing_tier_price",
            Some("priority"),
            true,
            recent,
            recent,
        ),
        (
            "batch-fast",
            &*model,
            Some(a),
            "missing_tier_price",
            Some("fast"),
            true,
            recent,
            recent,
        ),
        (
            "old-price",
            &*model,
            Some(a),
            "missing_tier_price",
            Some("priority"),
            false,
            recent,
            at - TimeDelta::hours(25),
        ),
        (
            "future-price",
            &*model,
            Some(a),
            "missing_tier_price",
            Some("priority"),
            false,
            recent,
            at + TimeDelta::days(1),
        ),
        (
            "corrected",
            &*model,
            Some(a),
            "missing_tier_price",
            Some("priority"),
            false,
            recent,
            recent,
        ),
        (
            "other-model",
            &*other,
            Some(a),
            "missing_tier_price",
            Some("priority"),
            false,
            recent,
            recent,
        ),
        (
            "group-a",
            &*model,
            Some(a),
            "missing_rate",
            None,
            false,
            recent,
            recent,
        ),
        (
            "group-b",
            &*model,
            Some(b),
            "missing_rate",
            None,
            false,
            recent,
            recent,
        ),
        (
            "group-null",
            &*model,
            None,
            "missing_rate",
            None,
            false,
            recent,
            recent,
        ),
        (
            "unnamed-model",
            "",
            Some(a),
            "missing_price",
            None,
            false,
            recent,
            recent,
        ),
        (
            "named-model",
            &*model,
            Some(a),
            "missing_price",
            None,
            false,
            recent,
            recent,
        ),
    ] {
        row(&w, (id, m, group, code, tier, batch, occurred, priced)).await;
    }
    corrected(&w, "corrected").await;
    let base = vec![
        ("status".to_string(), "unpriced".to_string()),
        (
            "priced_since".to_string(),
            (at - TimeDelta::hours(24)).to_rfc3339(),
        ),
        ("priced_before".to_string(), at.to_rfc3339()),
        ("exclude_corrected".to_string(), "true".to_string()),
    ];
    for (code, model_id, field, value, expected) in [
        (
            "missing_tier_price",
            &*model,
            "pricing_tier",
            "priority".to_string(),
            vec!["priority", "late"],
        ),
        (
            "missing_tier_price",
            &*model,
            "pricing_tier",
            "fast".to_string(),
            vec!["fast"],
        ),
        (
            "missing_tier_price",
            &*model,
            "pricing_tier",
            "batch".to_string(),
            vec!["batch-priority", "batch-fast"],
        ),
        (
            "missing_tier_price",
            &*model,
            "pricing_tier",
            "".to_string(),
            vec!["missing", "empty"],
        ),
        (
            "missing_rate",
            &*model,
            "group_id",
            a.to_string(),
            vec!["group-a"],
        ),
        (
            "missing_rate",
            &*model,
            "group_id",
            b.to_string(),
            vec!["group-b"],
        ),
        (
            "missing_rate",
            &*model,
            "group_id",
            "".to_string(),
            vec!["group-null"],
        ),
        (
            "missing_price",
            "",
            "model_id",
            "".to_string(),
            vec!["unnamed-model"],
        ),
    ] {
        let mut q = base.clone();
        q.extend([
            ("error_code".to_string(), code.to_string()),
            ("model_id".to_string(), model_id.to_string()),
        ]);
        // 空模型的筛选已经在上面，不传重复的同名参数。
        if field != "model_id" {
            q.push((field.to_string(), value));
        }
        let expected: BTreeSet<String> = expected.into_iter().map(str::to_string).collect();
        assert_eq!(ids(&get(&w, &q).await), expected, "{q:?}");
    }
    let mut q = base;
    q.extend([
        ("model_id".to_string(), model.clone()),
        ("error_code".to_string(), "missing_tier_price".to_string()),
        ("pricing_tier".to_string(), "priority".to_string()),
        ("limit".to_string(), "1".to_string()),
    ]);
    let first = get(&w, &q).await;
    let before = first["next_before"].as_i64().expect("还有一条匹配明细");
    q.push(("before".to_string(), before.to_string()));
    let last = get(&w, &q).await;
    assert_eq!(
        ids(&first)
            .union(&ids(&last))
            .cloned()
            .collect::<BTreeSet<_>>(),
        ["late".to_string(), "priority".to_string()].into()
    );
    assert_eq!(last["next_before"], Value::Null);

    let r =
        w.cp.send("GET", "/admin/pricing/status", Some(w.admin.cookie()), None)
            .await;
    assert_eq!(r.status, 200, "{r:?}");
    let since = r.body["window"]["since"]
        .as_str()
        .expect("显式返回计价窗口起点");
    let until = r.body["window"]["until"]
        .as_str()
        .expect("显式返回计价窗口终点");
    assert_eq!(
        DateTime::parse_from_rfc3339(until).expect("终点")
            - DateTime::parse_from_rfc3339(since).expect("起点"),
        TimeDelta::hours(24)
    );
    for detail in r.body["unpriced_details"]["items"]
        .as_array()
        .expect("状态明细")
    {
        let code = detail["code"].as_str().expect("原因");
        let mut q = vec![
            ("status".to_string(), "unpriced".to_string()),
            ("exclude_corrected".to_string(), "true".to_string()),
            ("priced_since".to_string(), since.to_string()),
            ("priced_before".to_string(), until.to_string()),
            ("error_code".to_string(), code.to_string()),
            (
                "model_id".to_string(),
                detail["model"]["id"].as_str().unwrap_or("").to_string(),
            ),
        ];
        if code == "missing_tier_price" {
            q.push((
                "pricing_tier".to_string(),
                detail["service_tier"].as_str().expect("档位").to_string(),
            ));
        }
        if code == "missing_rate" {
            q.push((
                "group_id".to_string(),
                detail["group"]["id"].as_str().unwrap_or("").to_string(),
            ));
        }
        assert_eq!(
            ids(&get(&w, &q).await).len() as u64,
            detail["count"].as_u64().expect("显式次数"),
            "{detail}"
        );
    }
}

#[tokio::test]
async fn pricing_drilldown_rejects_invalid_group_and_pricing_window() {
    let Some(w) = world().await else { return };
    for query in [
        "group_id=not-a-uuid",
        "priced_since=2026-11-10T00:00:00Z&priced_before=2026-11-09T00:00:00Z",
        "exclude_corrected=maybe",
    ] {
        let r =
            w.cp.send(
                "GET",
                &format!("/admin/pricing/charges?{query}"),
                Some(w.admin.cookie()),
                None,
            )
            .await;
        assert_eq!(r.status, 422, "{query}: {r:?}");
    }
}

#[tokio::test]
async fn pricing_window_includes_its_start_and_excludes_its_end() {
    let Some(w) = world().await else { return };
    let at = db_now(&w.cp).await;
    let since = at - TimeDelta::hours(1);
    for (id, priced) in [
        ("before", since - TimeDelta::microseconds(1)),
        ("start", since),
        ("end", at),
    ] {
        row(
            &w,
            (id, "", None, "missing_price", None, false, since, priced),
        )
        .await;
    }
    let q = vec![
        ("priced_since".to_string(), since.to_rfc3339()),
        ("priced_before".to_string(), at.to_rfc3339()),
    ];
    assert_eq!(ids(&get(&w, &q).await), ["start".to_string()].into());
}
