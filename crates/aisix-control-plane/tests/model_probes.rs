//! 真实控制平面上的探测权限和目标边界；这些场景不向上游发出请求。

mod support;

use aisix_control_plane::auth::OperatorRole;
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

async fn probe(cp: &TestCp, who: Option<&OperatorSession>, id: Uuid) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/models/{id}/probe"),
        who.map(OperatorSession::cookie),
        Some(json!({})),
    )
    .await
}

#[tokio::test]
async fn only_an_admin_can_spend_a_models_credential() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let id = Uuid::new_v4();
    let anonymous = probe(&cp, None, id).await;
    assert_eq!(anonymous.status, 401, "{anonymous:?}");
    for role in [OperatorRole::Viewer, OperatorRole::Operator] {
        let caller = cp.admin_login_seeded(role).await;
        let refused = probe(&cp, Some(&caller), id).await;
        assert_eq!(refused.status, 403, "{role:?}: {refused:?}");
    }
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let missing = probe(&cp, Some(&admin), id).await;
    assert_eq!(missing.status, 404, "{missing:?}");
}

async fn model(cp: &TestCp, admin: &OperatorSession, doc: Value) -> Uuid {
    let created = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": doc})),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    serde_json::from_value(created.body["id"].clone()).expect("模型 UUID")
}

#[tokio::test]
async fn a_composite_is_not_a_concrete_direct_target() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = model(
        &cp,
        &admin,
        json!({
            "display_name": "probe-routing",
            "routing": {"strategy": "round_robin", "targets": []}
        }),
    )
    .await;
    let result = probe(&cp, Some(&admin), id).await;
    assert_eq!(result.status, 200, "{result:?}");
    assert_eq!(result.body["model_id"], id.to_string());
    assert_eq!(result.body["status"], "unsupported");
    assert!(result.body.get("ttft_ms").is_some());
    assert_eq!(result.body["ttft_ms"], Value::Null);
    assert_eq!(result.body["usage"], Value::Null);
    assert_eq!(result.body["fidelity"]["status"], "not_tested");
}

async fn direct_model(cp: &TestCp, admin: &OperatorSession, display_name: &str) -> Uuid {
    let account = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": format!("probe-account-{}", Uuid::new_v4()),
                "provider": "openai", "api_key": "sk-probe-secret-never-sent-42",
                "api_base": "http://operator:secret-password@127.0.0.1:1/v1", "groups": ["shared"]
            }})),
        )
        .await;
    assert_eq!(account.status, 201, "{account:?}");
    model(
        cp,
        admin,
        json!({
            "display_name": display_name, "provider": "openai", "model_name": "concrete-upstream",
            "provider_key_id": account.body["id"]
        }),
    )
    .await
}

#[tokio::test]
async fn a_wildcard_never_substitutes_a_guessed_model() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "probe-*").await;
    let result = probe(&cp, Some(&admin), id).await;
    assert_eq!(result.status, 200, "{result:?}");
    assert_eq!(result.body["status"], "unsupported");
    assert_eq!(result.body["http_status"], Value::Null);
    assert_eq!(result.body["ttft_ms"], Value::Null);
}

#[tokio::test]
async fn a_blocked_target_is_failed_without_exposing_its_credentials() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "probe-concrete").await;
    let result = probe(&cp, Some(&admin), id).await;
    assert_eq!(result.status, 200, "{result:?}");
    assert_eq!(result.body["status"], "failed");
    for field in [
        "http_status",
        "ttft_ms",
        "usage",
        "output_tokens_per_second",
    ] {
        assert!(result.body.get(field).is_some(), "{field} 必须显式存在");
        assert_eq!(result.body[field], Value::Null);
    }
    let serialized = result.body.to_string();
    assert!(!serialized.contains("sk-probe-secret-never-sent-42"));
    assert!(!serialized.contains("secret-password"));
    assert!(!serialized.contains("operator:secret"));
    assert!(result.body["error"]
        .as_str()
        .is_some_and(|message| !message.is_empty()));
}

#[tokio::test]
async fn capability_checks_are_opt_in_and_never_follow_a_failed_base_probe() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "probe-capability-gate").await;
    for body in [json!({}), json!({"capability_check": true})] {
        let result = cp
            .send(
                "POST",
                &format!("/admin/models/{id}/probe"),
                Some(admin.cookie()),
                Some(body),
            )
            .await;
        assert_eq!(result.status, 200, "{result:?}");
        assert_eq!(result.body["status"], "failed");
        let capability = &result.body["capability_check"];
        assert_eq!(capability["status"], "not_tested");
        for field in ["total_duration_ms", "usage", "error"] {
            assert!(capability.get(field).is_some(), "{field} 必须显式存在");
            assert_eq!(capability[field], Value::Null);
        }
        for field in ["json", "instruction", "arithmetic", "sorting"] {
            assert!(capability["checks"].get(field).is_some());
            assert_eq!(capability["checks"][field], Value::Null);
        }
    }
}

#[tokio::test]
async fn no_probe_is_queued_when_all_execution_slots_are_held() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "probe-busy").await;
    let slots = cp
        .state()
        .model_probes
        .acquire_many(3)
        .await
        .expect("占满探测额度");
    let refused = probe(&cp, Some(&admin), id).await;
    assert_eq!(refused.status, 503, "{refused:?}");
    assert_eq!(refused.body["code"], "busy");
    drop(slots);
    let admitted = probe(&cp, Some(&admin), id).await;
    assert_eq!(
        admitted.status, 200,
        "释放额度后应执行地址检查：{admitted:?}"
    );
    assert_eq!(admitted.body["status"], "failed");
}

async fn history(cp: &TestCp, who: Option<&OperatorSession>, query: &str) -> Resp {
    cp.send(
        "GET",
        &format!("/admin/model-probes{query}"),
        who.map(OperatorSession::cookie),
        None,
    )
    .await
}

// 删除持久写入、只放内存、漏账号版本或把 Viewer 挡在外面都应失败。
#[tokio::test]
async fn saved_probe_survives_a_new_control_plane_and_configuration_changes() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let id = direct_model(&cp, &admin, "history-concrete").await;
    let result = probe(&cp, Some(&admin), id).await;
    assert_eq!(result.status, 200, "{result:?}");
    let page = history(&cp, Some(&viewer), &format!("?model_id={id}&limit=1")).await;
    assert_eq!(page.status, 200, "{page:?}");
    let item = &page.body["items"][0];
    assert!(item["id"].is_string());
    assert!(item["model_version"].is_string());
    assert!(item["provider_key_version"].is_string());
    assert_eq!(item["result"], result.body);
    assert_eq!(item["state"], "completed");
    assert_eq!(item.get("stale"), Some(&json!(false)));
    assert_eq!(item["mode"], "baseline");
    assert_eq!(item["actor"]["id"], admin.id.to_string());
    let restarted = cp.restart().await;
    let reread = history(&restarted, Some(&viewer), &format!("?model_id={id}")).await;
    assert_eq!(reread.status, 200, "{reread:?}");
    assert_eq!(reread.body["items"][0]["id"], item["id"]);
    let account_id: Uuid = serde_json::from_value(item["provider_key_id"].clone()).expect("账号id");
    sqlx::query(
        "UPDATE gw_resources SET version = version + 1 WHERE kind = 'provider_keys' AND id = $1",
    )
    .bind(account_id)
    .execute(cp.db().pool())
    .await
    .expect("只改变账号版本");
    let changed = history(&cp, Some(&viewer), &format!("?model_id={id}")).await;
    assert_eq!(changed.body["items"][0].get("stale"), Some(&json!(true)));
    assert_eq!(changed.body["items"][0]["result"], result.body);
    sqlx::query(
        "UPDATE gw_resources SET version = version - 1 WHERE kind = 'provider_keys' AND id = $1",
    )
    .bind(account_id)
    .execute(cp.db().pool())
    .await
    .expect("恢复账号版本以隔离模型变更");
    let restored = history(&cp, Some(&viewer), &format!("?model_id={id}")).await;
    assert_eq!(restored.body["items"][0].get("stale"), Some(&json!(false)));
    sqlx::query("UPDATE gw_resources SET version = version + 1 WHERE kind = 'models' AND id = $1")
        .bind(id)
        .execute(cp.db().pool())
        .await
        .expect("只改变模型版本");
    let model_changed = history(&cp, Some(&viewer), &format!("?model_id={id}")).await;
    assert_eq!(
        model_changed.body["items"][0].get("stale"),
        Some(&json!(true))
    );
}

#[tokio::test]
async fn saved_probes_are_operator_read_only_and_the_query_is_bounded() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    assert_eq!(history(&cp, None, "").await.status, 401);
    let tenant = cp
        .seed_tenant(aisix_control_plane::tenants::TenantStatus::Active)
        .await;
    let member = cp
        .member_login_seeded(tenant, aisix_control_plane::auth::MemberRole::Owner)
        .await;
    let tenant_read = cp
        .send("GET", "/admin/model-probes", Some(member.cookie()), None)
        .await;
    assert_eq!(tenant_read.status, 401, "租户会话不可读内部测试历史");
    for role in [
        OperatorRole::Viewer,
        OperatorRole::Operator,
        OperatorRole::Admin,
    ] {
        let who = cp.admin_login_seeded(role).await;
        let page = history(&cp, Some(&who), "?limit=1000").await;
        assert_eq!(page.status, 200, "{role:?}: {page:?}");
        assert_eq!(page.body["items"], json!([]));
        assert_eq!(page.body.get("next_before"), Some(&Value::Null));
        assert_eq!(history(&cp, Some(&who), "?mode=made-up").await.status, 400);
        assert_eq!(
            history(&cp, Some(&who), "?status=made-up").await.status,
            400
        );
    }
}

// 游标不重复、mode 在 latest 前、基础状态在 latest 后；缺失值不能变成失败答案。
#[tokio::test]
async fn history_keeps_null_checks_and_independent_modes_with_real_cursors() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "history-filter-*").await;
    assert_eq!(probe(&cp, Some(&admin), id).await.status, 200);
    let cap = cp
        .send(
            "POST",
            &format!("/admin/models/{id}/probe"),
            Some(admin.cookie()),
            Some(json!({"capability_check": true})),
        )
        .await;
    assert_eq!(cap.status, 200, "{cap:?}");
    let first = history(&cp, Some(&admin), &format!("?model_id={id}&limit=1")).await;
    assert_eq!(first.status, 200, "{first:?}");
    assert_eq!(first.body["items"].as_array().expect("数组").len(), 1);
    assert_eq!(first.body["items"][0]["mode"], "capability");
    let cursor = first.body["next_before"].as_str().expect("下一页游标");
    let second = history(
        &cp,
        Some(&admin),
        &format!("?model_id={id}&limit=1&before={cursor}"),
    )
    .await;
    assert_eq!(second.body["items"][0]["mode"], "baseline");
    assert_ne!(second.body["items"][0]["id"], first.body["items"][0]["id"]);
    assert_eq!(second.body.get("next_before"), Some(&Value::Null));
    let latest_baseline = history(
        &cp,
        Some(&admin),
        &format!("?model_id={id}&latest=true&mode=baseline"),
    )
    .await;
    assert_eq!(
        latest_baseline.body["items"][0]["id"],
        second.body["items"][0]["id"]
    );
    // 明确的存储层合成样本：验证 JSON 三值读取，不冒充任何真实模型测量。
    let old_id: i64 = second.body["items"][0]["id"]
        .as_str()
        .expect("id")
        .parse()
        .expect("bigint");
    sqlx::query("UPDATE model_probe_history SET result = jsonb_set(jsonb_set(result, '{status}', '\"available\"'), '{capability_check,checks}', '{\"json\":true,\"instruction\":false,\"arithmetic\":null,\"sorting\":null}') WHERE id = $1")
        .bind(old_id).execute(cp.db().pool()).await.expect("存储层三值样本");
    let unavailable = history(
        &cp,
        Some(&admin),
        &format!("?model_id={id}&latest=true&status=available"),
    )
    .await;
    assert_eq!(
        unavailable.body["items"],
        json!([]),
        "不能用老成功代替最近失败"
    );
    let stored = history(&cp, Some(&admin), &format!("?model_id={id}&mode=baseline")).await;
    let checks = &stored.body["items"][0]["result"]["capability_check"]["checks"];
    assert_eq!(checks.get("json"), Some(&json!(true)));
    assert_eq!(checks.get("instruction"), Some(&json!(false)));
    assert_eq!(checks.get("arithmetic"), Some(&Value::Null));
    assert_eq!(checks.get("sorting"), Some(&Value::Null));
    sqlx::query("UPDATE model_probe_history SET started_at = now() - interval '61 seconds', finished_at = NULL WHERE id = $1")
        .bind(old_id).execute(cp.db().pool()).await.expect("模拟重启遗留行");
    let interrupted = history(&cp, Some(&admin), &format!("?model_id={id}&mode=baseline")).await;
    assert_eq!(interrupted.body["items"][0]["state"], "interrupted");
    assert_eq!(
        interrupted.body["items"][0]["result"]["capability_check"]["checks"],
        checks.clone()
    );
    let raw = interrupted.body.to_string();
    for secret in [
        "sk-probe-secret-never-sent-42",
        "secret-password",
        "operator:secret",
        "Reply with the word",
    ] {
        assert!(!raw.contains(secret), "历史不得包含 {secret}");
    }
}

// 仅以数据库锁延迟保存，无假上游或模型生成；客户端断连不能取消已接受的保存。
#[tokio::test]
async fn an_accepted_probe_finishes_saving_after_the_client_disconnects() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "history-disconnect-concrete").await;
    sqlx::raw_sql("CREATE FUNCTION hold_probe_save() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.finished_at IS NOT NULL THEN PERFORM pg_advisory_xact_lock(736381); END IF; RETURN NEW; END $$; CREATE TRIGGER hold_probe_save BEFORE UPDATE ON model_probe_history FOR EACH ROW EXECUTE FUNCTION hold_probe_save();")
        .execute(cp.db().pool()).await.expect("创建测试保存门");
    let mut gate = cp.db().pool().acquire().await.expect("门连接");
    sqlx::query("SELECT pg_advisory_lock(736381)")
        .execute(&mut *gate)
        .await
        .expect("关门");
    let url = cp.url(&format!("/admin/models/{id}/probe"));
    let cookie = admin.cookie().to_string();
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .header("Cookie", cookie)
            .json(&json!({"capability_check": true}))
            .send()
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let accepted: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM model_probe_history WHERE model_id = $1 AND finished_at IS NULL AND result IS NOT NULL)")
                .bind(id).fetch_one(cp.db().pool()).await.expect("等待接受记录");
            if accepted { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("先持久接受再允许断连");
    let partial = history(&cp, Some(&admin), &format!("?model_id={id}")).await;
    assert_eq!(partial.body["items"][0]["state"], "running");
    assert_eq!(partial.body["items"][0]["result"]["status"], "failed");
    assert_eq!(
        partial.body["items"][0]["result"]["capability_check"]["checks"].get("arithmetic"),
        Some(&Value::Null)
    );
    request.abort();
    let _ = request.await;
    sqlx::query("SELECT pg_advisory_unlock(736381)")
        .execute(&mut *gate)
        .await
        .expect("开门");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let page = history(&cp, Some(&admin), &format!("?model_id={id}")).await;
            if page.body["items"][0]["state"] == "completed" {
                assert_eq!(page.body["items"][0]["result"]["status"], "failed");
                assert_eq!(
                    page.body["items"].as_array().expect("数组").len(),
                    1,
                    "不得自动重试"
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("断连后有限任务仍完成保存");
}

// 去掉后端100条上限、游标误用<=或少取一行判页，都会在真实101条样本上失败。
#[tokio::test]
async fn saved_history_default_and_maximum_pages_never_repeat_or_omit_a_record() {
    use futures::{stream, StreamExt};
    use std::collections::BTreeSet;
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = direct_model(&cp, &admin, "history-pagination-*").await;
    let results = stream::iter(0..101)
        .map(|_| probe(&cp, Some(&admin), id))
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await;
    assert_eq!(results.len(), 101);
    for result in results {
        assert_eq!(result.status, 200, "{result:?}");
        assert_eq!(result.body["status"], "unsupported", "通配目标不调用上游");
    }
    let first = history(&cp, Some(&admin), &format!("?model_id={id}")).await;
    assert_eq!(first.status, 200, "{first:?}");
    assert_eq!(first.body["items"].as_array().expect("默认页").len(), 50);
    let mut all = BTreeSet::new();
    let mut page = first;
    loop {
        for record in page.body["items"].as_array().expect("记录列表") {
            let record_id = record["id"].as_str().expect("字符串id").to_string();
            assert!(all.insert(record_id), "游标页不能重复记录");
        }
        let Some(before) = page.body["next_before"].as_str() else {
            break;
        };
        page = history(
            &cp,
            Some(&admin),
            &format!("?model_id={id}&before={before}"),
        )
        .await;
        assert_eq!(page.status, 200, "{page:?}");
    }
    assert_eq!(all.len(), 101, "默认50条逐页不能遗漏记录");
    let maximum = history(&cp, Some(&admin), &format!("?model_id={id}&limit=1000")).await;
    assert_eq!(maximum.status, 200, "{maximum:?}");
    assert_eq!(
        maximum.body["items"].as_array().expect("上限页").len(),
        100,
        "客户端1000仍只返回上限100"
    );
    let before = maximum.body["next_before"]
        .as_str()
        .expect("第101条仍有下一页");
    let last = history(
        &cp,
        Some(&admin),
        &format!("?model_id={id}&limit=1000&before={before}"),
    )
    .await;
    assert_eq!(last.body["items"].as_array().expect("末页").len(), 1);
    assert_eq!(last.body.get("next_before"), Some(&Value::Null));
    let mut bounded = BTreeSet::new();
    for record in maximum.body["items"]
        .as_array()
        .expect("上限页")
        .iter()
        .chain(last.body["items"].as_array().expect("末页"))
    {
        assert!(bounded.insert(record["id"].as_str().expect("id").to_string()));
    }
    assert_eq!(bounded, all, "不同页大小均完整且不重复");
}
