//! 管理列表显式分页保持旧排序，筛选与游标贯穿每一页。
mod support;

use aisix_control_plane::auth::OperatorRole;
use serde_json::{json, Value};
use support::TestCp;
use uuid::Uuid;

async fn page(cp: &TestCp, cookie: &str, path: &str) -> Value {
    let r = cp.get_with_cookie(path, cookie).await;
    assert_eq!(r.status, 200, "{path}: {r:?}");
    r.body
}

#[tokio::test]
async fn tenant_pages_keep_time_id_order_and_status_scope() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Admin).await;
    let mut expected = Vec::new();
    for i in 1..=7 {
        let id = Uuid::from_u128(i);
        sqlx::query("INSERT INTO tenants (id,name,status,created_at) VALUES ($1,$2,$3,'2026-01-01T00:00:00Z')")
            .bind(id).bind(format!("page-{i}"))
            .bind(if i % 2 == 0 { "pending" } else { "active" })
            .execute(cp.db().pool()).await.expect("seed tenant");
        if i % 2 == 0 {
            expected.push(id.to_string());
        }
    }
    let legacy = page(&cp, op.cookie(), "/admin/tenants").await;
    assert!(legacy["items"].as_array().expect("items").len() >= 7);
    let mut found = Vec::new();
    let mut path = "/admin/tenants?status=pending&limit=1".to_string();
    for n in 0..3 {
        let body = page(&cp, op.cookie(), &path).await;
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), 1, "SQL must bound the page");
        assert_eq!(items[0]["status"], "pending");
        found.push(items[0]["id"].as_str().expect("id").to_string());
        assert!(
            body.get("next_before").is_some(),
            "cursor field is required"
        );
        if n < 2 {
            let cursor = body["next_before"]
                .as_str()
                .expect("more rows require a cursor");
            path = format!("/admin/tenants?status=pending&limit=1&before={cursor}");
        } else {
            assert!(body["next_before"].is_null());
        }
    }
    assert_eq!(
        found, expected,
        "equal timestamps may not duplicate or skip ids"
    );
    let empty = page(&cp, op.cookie(), "/admin/tenants?status=suspended&limit=1").await;
    assert_eq!(empty["items"], json!([]));
    assert!(empty.get("next_before").is_some_and(Value::is_null));
    for path in [
        "/admin/tenants?limit=0".to_string(),
        "/admin/tenants?limit=201".to_string(),
        "/admin/tenants?limit=-1".to_string(),
        "/admin/tenants?limit=1&before=no-cursor".to_string(),
        format!("/admin/tenants?limit=1&before={}", Uuid::new_v4()),
        format!(
            "/admin/tenants?status=pending&limit=1&before={}",
            Uuid::from_u128(1)
        ),
    ] {
        let r = cp.get_with_cookie(&path, op.cookie()).await;
        assert_eq!(r.status, 422, "invalid page parameters: {path}: {r:?}");
    }
}

#[tokio::test]
async fn resource_pages_keep_identity_order_and_kind_scope() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Admin).await;
    let mut expected = Vec::new();
    for name in [
        "page-charlie",
        "page-alpha",
        "page-echo",
        "page-bravo",
        "page-delta",
    ] {
        let r = cp.send("POST", "/admin/resources/rate_limit_policies", Some(op.cookie()), Some(json!({"doc":{
            "name":name,"conditions":[{"dimension":"team","operator":"==","value":"paging-team"}],
            "group_by":["team"],"limits":{"rpm":60}
        }}))).await;
        assert_eq!(r.status, 201, "{r:?}");
        expected.push((
            name.to_string(),
            r.body["id"].as_str().expect("id").to_string(),
        ));
    }
    expected.sort();
    let tombstone = cp.send("POST", "/admin/resources/rate_limit_policies", Some(op.cookie()), Some(json!({"doc":{
        "name":"page-deleted","conditions":[{"dimension":"team","operator":"==","value":"paging-team"}],
        "group_by":["team"],"limits":{"rpm":60}
    }}))).await;
    assert_eq!(tombstone.status, 201, "{tombstone:?}");
    let deleted_id = tombstone.body["id"].as_str().expect("deleted id");
    let deleted = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("HTTP client")
        .delete(cp.url(&format!(
            "/admin/resources/rate_limit_policies/{deleted_id}"
        )))
        .header(reqwest::header::COOKIE, op.cookie())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::IF_MATCH, "\"1\"")
        .send()
        .await
        .expect("delete the policy");
    assert_eq!(deleted.status(), 200);
    let legacy = page(&cp, op.cookie(), "/admin/resources/rate_limit_policies").await;
    assert_eq!(legacy["items"].as_array().expect("items").len(), 5);
    let mut found = Vec::new();
    let mut path = "/admin/resources/rate_limit_policies?limit=2".to_string();
    for n in 0..3 {
        let body = page(&cp, op.cookie(), &path).await;
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), if n < 2 { 2 } else { 1 });
        for item in items {
            found.push((
                item["doc"]["name"].as_str().expect("name").to_string(),
                item["id"].as_str().expect("id").to_string(),
            ));
        }
        assert!(body.get("next_before").is_some());
        if n < 2 {
            path = format!(
                "/admin/resources/rate_limit_policies?limit=2&before={}",
                body["next_before"].as_str().expect("cursor")
            );
        } else {
            assert!(body["next_before"].is_null());
        }
    }
    assert_eq!(found, expected);
    let empty = page(&cp, op.cookie(), "/admin/resources/models?limit=1").await;
    assert_eq!(empty["items"], json!([]));
    assert!(empty.get("next_before").is_some_and(Value::is_null));
    for path in [
        "/admin/resources/rate_limit_policies?limit=0".to_string(),
        "/admin/resources/rate_limit_policies?limit=201".to_string(),
        "/admin/resources/rate_limit_policies?before=no-cursor".to_string(),
        format!(
            "/admin/resources/rate_limit_policies?limit=1&before={}",
            Uuid::new_v4()
        ),
        format!("/admin/resources/models?limit=1&before={}", expected[0].1),
        format!("/admin/resources/rate_limit_policies?limit=1&before={deleted_id}"),
    ] {
        let r = cp.get_with_cookie(&path, op.cookie()).await;
        assert_eq!(r.status, 422, "invalid page parameters: {path}: {r:?}");
    }
}

#[tokio::test]
async fn resource_pages_with_no_identity_keep_id_order() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Admin).await;
    let initial = page(&cp, op.cookie(), "/admin/resources/teams").await;
    let mut expected: Vec<String> = initial["items"]
        .as_array()
        .expect("teams")
        .iter()
        .map(|row| row["id"].as_str().expect("id").to_string())
        .collect();
    for name in ["null-page-a", "null-page-b", "null-page-c"] {
        let r = cp
            .send(
                "POST",
                "/admin/tenants",
                Some(op.cookie()),
                Some(json!({"name": name})),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
        expected.push(r.body["id"].as_str().expect("tenant id").to_string());
    }
    expected.sort();
    let mut found = Vec::new();
    let mut path = "/admin/resources/teams?limit=1".to_string();
    for n in 0..expected.len() {
        let body = page(&cp, op.cookie(), &path).await;
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        found.push(items[0]["id"].as_str().expect("team id").to_string());
        if n + 1 < expected.len() {
            path = format!(
                "/admin/resources/teams?limit=1&before={}",
                body["next_before"].as_str().expect("cursor")
            );
        } else {
            assert!(body["next_before"].is_null());
        }
    }
    assert_eq!(found, expected);
}
