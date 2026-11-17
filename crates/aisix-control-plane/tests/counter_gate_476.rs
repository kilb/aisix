//! 明确退役是操作员确认，离线本身不能改变计数配置的兼容性判断。
mod support;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::error::ApiError;
use aisix_control_plane::resources::write::{write_resource, ResourceWrite, WriteCtx, WriteOp};
use aisix_control_plane::resources::Kind;
use futures::FutureExt;
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp};

const HOTFIX: &str = "dev+sha-a11ce11000000000000000000000000000000000";
const CAPABLE: &str = "dev+sha-d887d328e6a033caf26ac7d6bbcae86efef1d5b2";
const OLD: &str = "dev+sha-641ccf95bc96569e565d23845c9c3f612c662703";

async fn create(cp: &TestCp, admin: &OperatorSession, kind: &str, doc: Value) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/resources/{kind}"),
        Some(admin.cookie()),
        Some(json!({"doc": doc})),
    )
    .await
}

async fn settings(cp: &TestCp, admin: &OperatorSession) -> Value {
    settings_for(cp, admin, "messages_count_tokens").await
}

async fn settings_for(cp: &TestCp, admin: &OperatorSession, field: &str) -> Value {
    let provider = if field == "messages_count_tokens" {
        "anthropic"
    } else {
        "openai"
    };
    let account = create(
        cp,
        admin,
        "provider_keys",
        json!({"display_name":"counter", "provider":provider, "api_key":"test-only-counter"}),
    )
    .await;
    assert_eq!(account.status, 201, "{account:?}");
    let model = create(cp, admin, "models", json!({"display_name":"source", "provider":provider, "provider_key_id":account.body["id"], "model_name":"source-v1"})).await;
    assert_eq!(model.status, 201, "{model:?}");
    json!({field:{"provider_key_id":account.body["id"], "model_mappings":[{"model_id":model.body["id"], "source_model_name":"source-v1", "model_name":"counter-v1"}]}})
}

async fn beat(cp: &TestCp, admin: &OperatorSession, version: &str) -> String {
    let bundle = cp.issue_bundle(admin, "counter476").await;
    let r = cp.dp_client(&bundle).heartbeat(json!({"dp_id":bundle.dp_id, "instance_id":"counter476", "hostname":"counter476", "version":version, "uptime_seconds":7, "applied_revision":0, "supported_guardrail_kinds":[], "exporter_health":[], "usage_event_drops":{"queue_full":3}})).await;
    assert_eq!(r.status, 200, "{r:?}");
    bundle.dp_id
}

async fn age(cp: &TestCp, dp_id: &str) {
    let r = sqlx::query(
        "UPDATE dp_instances SET last_seen_at=now()-interval '23 hours' WHERE dp_id=$1::uuid",
    )
    .bind(dp_id)
    .execute(cp.db().pool())
    .await
    .expect("age owned instance");
    assert_eq!(r.rows_affected(), 1);
}

async fn retire(cp: &TestCp, admin: &OperatorSession, dp_id: &str, body: Value) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/gateways/instances/{dp_id}/counter476/retire"),
        Some(admin.cookie()),
        Some(body),
    )
    .await
}

fn refused(r: &Resp) {
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["code"], "gateway_too_old", "{r:?}");
    assert_eq!(
        r.body["details"][0]["path"], "/messages_count_tokens",
        "{r:?}"
    );
}

// 删除 d887 的独立能力基线会使热修控制面的保存返回 422。
#[tokio::test]
async fn verified_counter_build_survives_a_control_plane_only_hotfix() {
    let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let doc = settings(&cp, &admin).await;
    beat(&cp, &admin, CAPABLE).await;
    let r = create(&cp, &admin, "token_counting", doc).await;
    assert_eq!(r.status, 201, "{r:?}");
}

// 缺少显式退役接口或计数门禁仍统计退役行，会使第二次保存失败。
#[tokio::test]
async fn explicit_retirement_differs_from_twenty_three_hours_of_silence() {
    let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let doc = settings(&cp, &admin).await;
    let dp = beat(&cp, &admin, OLD).await;
    age(&cp, &dp).await;
    refused(&create(&cp, &admin, "token_counting", doc.clone()).await);
    let listed = cp
        .send(
            "GET",
            "/admin/gateways/instances",
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(listed.status, 200, "{listed:?}");
    assert_eq!(listed.body["items"][0]["online"], false, "{listed:?}");
    let r = retire(&cp, &admin, &dp, json!({"confirmed_stopped":true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body["retired_at"].as_str().is_some(), "{r:?}");
    assert_eq!(create(&cp, &admin, "token_counting", doc).await.status, 201);
}

async fn history(cp: &TestCp, dp: &str) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(i)-'retired_at' FROM dp_instances i WHERE dp_id=$1::uuid AND instance_id='counter476'")
        .bind(dp).fetch_one(cp.db().pool()).await.expect("read full instance history")
}

async fn retirement(cp: &TestCp, dp: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar(
        "SELECT retired_at FROM dp_instances WHERE dp_id=$1::uuid AND instance_id='counter476'",
    )
    .bind(dp)
    .fetch_one(cp.db().pool())
    .await
    .expect("read marker")
}

async fn audit_count(cp: &TestCp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action='gateway_instance.retire'")
        .fetch_one(cp.db().pool())
        .await
        .expect("count retirement audits")
}

// 去掉确认、角色或在线检查会使拒绝断言变红；更新历史字段或重复审计也会失败。
#[tokio::test]
async fn retirement_requires_admin_confirmation_and_preserves_history_idempotently() {
    let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = beat(&cp, &admin, OLD).await;
    let listed = cp
        .send(
            "GET",
            "/admin/gateways/instances",
            Some(admin.cookie()),
            None,
        )
        .await;
    assert!(
        listed.body["items"][0]
            .as_object()
            .expect("instance object")
            .contains_key("retired_at"),
        "{listed:?}"
    );
    assert!(
        listed.body["items"][0]["retired_at"].is_null(),
        "{listed:?}"
    );
    let r = retire(&cp, &admin, &dp, json!({"confirmed_stopped":true})).await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "gateway_instance_online", "{r:?}");
    age(&cp, &dp).await;
    for body in [json!({}), json!({"confirmed_stopped":false})] {
        let r = retire(&cp, &admin, &dp, body).await;
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(r.body["details"][0]["path"], "/confirmed_stopped", "{r:?}");
    }
    let r = retire(&cp, &operator, &dp, json!({"confirmed_stopped":true})).await;
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(audit_count(&cp).await, 0);
    assert!(retirement(&cp, &dp).await.is_none());
    let before = history(&cp, &dp).await;
    assert_eq!(before["version"], OLD);
    assert_eq!(before["usage_event_drops"]["queue_full"], 3);
    let first = retire(&cp, &admin, &dp, json!({"confirmed_stopped":true})).await;
    assert_eq!(first.status, 200, "{first:?}");
    assert_eq!(first.body["dp_id"], dp);
    assert_eq!(first.body["instance_id"], "counter476");
    assert!(first.body["retired_at"].as_str().is_some());
    let repeated = retire(&cp, &admin, &dp, json!({"confirmed_stopped":true})).await;
    assert_eq!(repeated.status, 200, "{repeated:?}");
    assert_eq!(repeated.body, first.body);
    assert_eq!(history(&cp, &dp).await, before);
    assert_eq!(audit_count(&cp).await, 1);
    let audit: Value =
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action='gateway_instance.retire'")
            .fetch_one(cp.db().pool())
            .await
            .expect("read audit");
    assert_eq!(
        audit,
        json!({"dp_id":dp, "instance_id":"counter476", "retired_at":first.body["retired_at"], "confirmed_stopped":true})
    );
    let listed = cp
        .send(
            "GET",
            "/admin/gateways/instances",
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(
        listed.body["items"][0]["retired_at"],
        first.body["retired_at"]
    );
    assert_eq!(listed.body["items"][0]["online"], false);
    let missing = retire(
        &cp,
        &admin,
        &uuid::Uuid::new_v4().to_string(),
        json!({"confirmed_stopped":true}),
    )
    .await;
    assert_eq!(missing.status, 404, "{missing:?}");
}

// 审计若移出事务，失败会留下没有审计记录的退役标记。
#[tokio::test]
async fn retirement_rolls_back_when_its_audit_cannot_be_written() {
    let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let dp = beat(&cp, &admin, OLD).await;
    age(&cp, &dp).await;
    sqlx::raw_sql("CREATE FUNCTION refuse_retirement_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='gateway_instance.retire' THEN RAISE EXCEPTION 'test retirement audit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER refuse_retirement_audit BEFORE INSERT ON audit_log FOR EACH ROW EXECUTE FUNCTION refuse_retirement_audit();")
        .execute(cp.db().pool()).await.expect("install isolated audit fault");
    let before = history(&cp, &dp).await;
    let r = retire(&cp, &admin, &dp, json!({"confirmed_stopped":true})).await;
    assert_eq!(r.status, 500, "{r:?}");
    assert!(retirement(&cp, &dp).await.is_none());
    assert_eq!(history(&cp, &dp).await, before);
    assert_eq!(audit_count(&cp).await, 0);
}

// 此语句与迁移前控制面的 heartbeat::record 完全同形，不提及新增列。
async fn legacy_heartbeat(conn: &mut sqlx::PgConnection, dp: &str) {
    sqlx::query(
        "INSERT INTO dp_instances (dp_id, instance_id, hostname, version, uptime_seconds, \
         applied_revision, config_hash, supported_guardrail_kinds, exporter_health, \
         rejected_resources, partially_compatible_resources, usage_event_drops) \
         VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
         ON CONFLICT (dp_id, instance_id) DO UPDATE SET hostname = EXCLUDED.hostname, \
         version = EXCLUDED.version, uptime_seconds = EXCLUDED.uptime_seconds, \
         applied_revision = EXCLUDED.applied_revision, config_hash = EXCLUDED.config_hash, \
         supported_guardrail_kinds = EXCLUDED.supported_guardrail_kinds, \
         exporter_health = EXCLUDED.exporter_health, \
         rejected_resources = EXCLUDED.rejected_resources, \
         partially_compatible_resources = EXCLUDED.partially_compatible_resources, \
         usage_event_drops = EXCLUDED.usage_event_drops, last_seen_at = now()",
    )
    .bind(dp)
    .bind("counter476")
    .bind("legacy")
    .bind(OLD)
    .bind(8_i64)
    .bind(0_i64)
    .bind(None::<String>)
    .bind(json!([]))
    .bind(json!([]))
    .bind(json!([]))
    .bind(json!([]))
    .bind(json!({"queue_full":3}))
    .execute(conn)
    .await
    .expect("execute exact legacy heartbeat upsert");
}

// 删除迁移触发器或改为比较两个时间，会让倒置时间的旧心跳继续被计数门禁忽略。
#[tokio::test]
async fn legacy_heartbeat_clears_retirement_even_when_transaction_time_goes_backwards() {
    let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let doc = settings(&cp, &admin).await;
    let dp = beat(&cp, &admin, OLD).await;
    age(&cp, &dp).await;
    let mut old_tx = cp
        .db()
        .pool()
        .begin()
        .await
        .expect("begin old heartbeat transaction");
    let started: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *old_tx)
        .await
        .expect("pin old transaction time");
    let r = retire(&cp, &admin, &dp, json!({"confirmed_stopped":true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let retired = retirement(&cp, &dp).await.expect("retired timestamp");
    assert!(started < retired, "test must invert the timestamps");
    assert_eq!(
        create(&cp, &admin, "token_counting", doc.clone())
            .await
            .status,
        201
    );
    legacy_heartbeat(&mut old_tx, &dp).await;
    old_tx
        .commit()
        .await
        .expect("commit legacy heartbeat after retirement");
    assert!(retirement(&cp, &dp).await.is_none());
    let after = history(&cp, &dp).await;
    let seen: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(after["last_seen_at"].clone()).expect("last seen");
    assert_eq!(seen, started);
    assert!(seen < retired);
    let mut changed = doc;
    changed["messages_count_tokens"]["model_mappings"][0]["model_name"] =
        json!("changed-after-return");
    let r = cp
        .send_conditional(
            "PUT",
            &format!("/admin/resources/token_counting/{}", cp.env().env_id),
            admin.cookie(),
            1,
            json!({"doc":changed}),
        )
        .await;
    refused(&r);
}

async fn wait_for_lock(cp: &TestCp, pattern: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND wait_event_type='Lock' AND query LIKE $1)")
            .bind(pattern).fetch_one(cp.db().pool()).await.expect("observe real PG lock wait");
        if waiting {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "query never waited: {pattern}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// 心跳先排队时退役必须重判在线；仅先读后无条件写会把恢复中的实例错误退役。
#[tokio::test]
async fn real_heartbeat_winning_the_row_lock_prevents_retirement() {
    let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = cp.issue_bundle(&admin, "race476").await;
    let client = cp.dp_client(&bundle);
    let body = json!({"dp_id":bundle.dp_id,"instance_id":"counter476","version":OLD});
    assert_eq!(client.heartbeat(body.clone()).await.status, 200);
    age(&cp, &bundle.dp_id).await;
    let mut blocker = cp.db().pool().begin().await.expect("lock instance");
    sqlx::query("SELECT 1 FROM dp_instances WHERE dp_id=$1::uuid FOR UPDATE")
        .bind(&bundle.dp_id)
        .execute(&mut *blocker)
        .await
        .expect("hold row before race");
    let heartbeat = client.heartbeat(body);
    let retirement = async {
        wait_for_lock(&cp, "%INSERT INTO dp_instances%ON CONFLICT%").await;
        retire(
            &cp,
            &admin,
            &bundle.dp_id,
            json!({"confirmed_stopped":true}),
        )
        .await
    };
    let release = async {
        wait_for_lock(&cp, "%SELECT last_seen_at > now()%FOR UPDATE%").await;
        blocker.rollback().await.expect("release race row");
    };
    let (beat, retired, ()) = tokio::join!(heartbeat, retirement, release);
    assert_eq!(beat.status, 200, "{beat:?}");
    assert_eq!(retired.status, 409, "{retired:?}");
    assert!(self::retirement(&cp, &bundle.dp_id).await.is_none());
    assert_eq!(audit_count(&cp).await, 0);
}

// 新进程 id 不继承退役，原 id 的真实心跳也必须清标并恢复保存拒绝。
#[tokio::test]
async fn real_returning_heartbeat_and_new_boot_do_not_inherit_retirement() {
    let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let doc = settings(&cp, &admin).await;
    let bundle = cp.issue_bundle(&admin, "return476").await;
    let client = cp.dp_client(&bundle);
    let mut body = json!({"dp_id":bundle.dp_id,"instance_id":"counter476","version":OLD});
    assert_eq!(client.heartbeat(body.clone()).await.status, 200);
    age(&cp, &bundle.dp_id).await;
    assert_eq!(
        retire(
            &cp,
            &admin,
            &bundle.dp_id,
            json!({"confirmed_stopped":true})
        )
        .await
        .status,
        200
    );
    assert_eq!(client.heartbeat(body.clone()).await.status, 200);
    assert!(retirement(&cp, &bundle.dp_id).await.is_none());
    refused(&create(&cp, &admin, "token_counting", doc.clone()).await);
    age(&cp, &bundle.dp_id).await;
    assert_eq!(
        retire(
            &cp,
            &admin,
            &bundle.dp_id,
            json!({"confirmed_stopped":true})
        )
        .await
        .status,
        200
    );
    body["instance_id"] = json!("new-boot476");
    assert_eq!(client.heartbeat(body).await.status, 200);
    let new_retired: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT retired_at FROM dp_instances WHERE dp_id=$1::uuid AND instance_id='new-boot476'",
    )
    .bind(&bundle.dp_id)
    .fetch_one(cp.db().pool())
    .await
    .expect("new boot marker");
    assert!(new_retired.is_none());
    refused(&create(&cp, &admin, "token_counting", doc).await);
}

// 发行门槛若仍读取未过滤清单会拒绝计数；若全局过滤则其他资源会错误放行。
#[tokio::test]
async fn retirement_filters_both_counter_judges_but_no_unrelated_floor() {
    let Some(cp) = TestCp::start_stamped("1.9.0+sha-a11ce11").await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let doc = settings(&cp, &admin).await;
    let dp = beat(&cp, &admin, "1.8.0+sha-c0ffee0").await;
    age(&cp, &dp).await;
    refused(&create(&cp, &admin, "token_counting", doc.clone()).await);
    assert_eq!(
        retire(&cp, &admin, &dp, json!({"confirmed_stopped":true}))
            .await
            .status,
        200
    );
    assert_eq!(create(&cp, &admin, "token_counting", doc).await.status, 201);
    let key = write_key(
        &cp,
        json!({"key_hash":"test-retirement-unrelated", "allowed_endpoints":["chat_completions"]}),
    )
    .await;
    let error = key.expect_err("retirement cannot waive an unrelated floor");
    assert_eq!(error.code, "gateway_too_old");
    assert_eq!(error.details[0].path, "/allowed_endpoints");
}

// 即使显式退役了全部可见实例，容量上限仍代表未被记录的实例可能存在。
#[tokio::test]
async fn retirement_never_erases_cap_hit_or_full_inventory_uncertainty() {
    for full in [false, true] {
        let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
            return;
        };
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let doc = settings(&cp, &admin).await;
        let dp = beat(&cp, &admin, OLD).await;
        age(&cp, &dp).await;
        assert_eq!(
            retire(&cp, &admin, &dp, json!({"confirmed_stopped":true}))
                .await
                .status,
            200
        );
        if full {
            sqlx::query("INSERT INTO dp_instances (dp_id,instance_id,version,last_seen_at,retired_at) SELECT $1::uuid,'full476-' || n,$2,now()-interval '23 hours',now() FROM generate_series(1,127) n")
                .bind(&dp).bind(OLD).execute(cp.db().pool()).await.expect("full retired fixture");
        } else {
            sqlx::query("UPDATE dp_credentials SET instance_cap_hit_at=now() WHERE dp_id=$1::uuid")
                .bind(&dp)
                .execute(cp.db().pool())
                .await
                .expect("record cap uncertainty");
        }
        refused(&create(&cp, &admin, "token_counting", doc).await);
        assert_eq!(
            create(&cp, &admin, "token_counting", json!({}))
                .await
                .status,
            201
        );
    }
}

// 扩大匹配到任意前缀/旧接口白名单，或漏掉任一已验产物，会分别破坏负例/正例。
#[tokio::test]
async fn independent_verified_counter_baselines_match_only_full_or_exact_seven_character_stamps() {
    let known = [
        "119796c3d1794a299835aa86ac324e90ba40e4fa",
        "5a2b3bc92cbfc71703d7ba1b2e2fe1e554c4c7a2",
        "d887d328e6a033caf26ac7d6bbcae86efef1d5b2",
    ];
    let mut cases: Vec<(String, bool)> = Vec::new();
    for sha in known {
        cases.push((format!("dev+sha-{sha}"), true));
        cases.push((format!("dev+sha-{}", &sha[..7]), true));
        cases.push((format!("dev+sha-{}", &sha[..8]), false));
        cases.push((format!("dev+sha-{}", &sha[..39]), false));
        cases.push((format!("dev+sha-{}0", &sha[..39]), false));
    }
    for version in [
        "dev",
        "unknown",
        "dev+sha-c0ffee0",
        "dev+sha-08ff931",
        "dev+sha-d5d5d72",
        "dev+sha-676463f",
        OLD,
        "dev+sha-119796c-tail",
        "dev+sha-119796",
        "dev+sha-a11ce11",
    ] {
        cases.push((version.into(), false));
    }
    for cp_version in [HOTFIX, "1.9.0+sha-a11ce11"] {
        for field in ["messages_count_tokens", "responses_input_tokens"] {
            let Some(cp) = TestCp::start_stamped(cp_version).await else {
                return;
            };
            let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
            let mut doc = settings_for(&cp, &admin, field).await;
            assert_eq!(
                create(&cp, &admin, "token_counting", json!({}))
                    .await
                    .status,
                201
            );
            let bundle = cp.issue_bundle(&admin, "matrix476").await;
            let client = cp.dp_client(&bundle);
            let mut version = 1_i64;
            for (index, (gateway, allowed)) in cases.iter().enumerate() {
                let r = client
                    .heartbeat(
                        json!({"dp_id":bundle.dp_id,"instance_id":"matrix476","version":gateway}),
                    )
                    .await;
                assert_eq!(r.status, 200, "{r:?}");
                doc[field]["model_mappings"][0]["model_name"] = json!(format!("counter-{index}"));
                let r = cp
                    .send_conditional(
                        "PUT",
                        &format!("/admin/resources/token_counting/{}", cp.env().env_id),
                        admin.cookie(),
                        version,
                        json!({"doc":doc}),
                    )
                    .await;
                if *allowed {
                    assert_eq!(
                        r.status, 200,
                        "cp={cp_version} field={field} gateway={gateway}: {r:?}"
                    );
                    assert_eq!(r.body["doc"], doc, "stored contract: {r:?}");
                    version += 1;
                } else {
                    assert_eq!(
                        r.status, 422,
                        "cp={cp_version} field={field} gateway={gateway}: {r:?}"
                    );
                    assert_eq!(r.body["code"], "gateway_too_old", "{r:?}");
                    assert_eq!(r.body["details"][0]["path"], format!("/{field}"), "{r:?}");
                }
            }
        }
    }
}

// 热修后 d887 的原生族与客户扩展族不能因控制面的新 SHA 被误拒。
#[tokio::test]
async fn verified_current_build_keeps_both_preexisting_endpoint_capabilities_after_hotfix() {
    let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    beat(&cp, &admin, CAPABLE).await;
    let version = write_key(&cp, json!({"key_hash":"counter476-endpoints", "allowed_model_ids":[], "allowed_endpoints":["images", "files", "conversations", "containers", "vector_stores", "skills", "evals"]})).await.expect("both preexisting endpoint families survive hotfix");
    assert_eq!(version, 1);
}

// 退役记录不能覆盖在线实例的能力拒绝，即使数据库中出现不一致的在线标记。
#[tokio::test]
async fn rollout_does_not_hide_online_incompatible_builds_with_retirement_markers() {
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    let Some(cp) = TestCp::start_with(|cfg| {
        cfg.cp_version = HOTFIX.into();
        cfg.kine_endpoint = endpoint.clone();
    })
    .await
    else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let doc = settings(&cp, &admin).await;
    assert_eq!(
        create(&cp, &admin, "token_counting", doc.clone())
            .await
            .status,
        201
    );
    let revision = projected_counter(&cp, &admin, &endpoint, &doc).await;
    let dp = beat(&cp, &admin, OLD).await;
    sqlx::query(
        "UPDATE dp_instances SET retired_at=now(), applied_revision=$2 WHERE dp_id=$1::uuid",
    )
    .bind(&dp)
    .bind(revision)
    .execute(cp.db().pool())
    .await
    .expect("seed inconsistent marker");
    let r = cp
        .send(
            "GET",
            &format!("/admin/resources/token_counting/{}", cp.env().env_id),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["rollout"]["state"], "rejected", "{r:?}");
    assert_eq!(r.body["rollout"]["online"], 1, "{r:?}");
    assert_eq!(r.body["rollout"]["applied"], 0, "{r:?}");
    assert!(
        r.body["rollout"]["rejections"]
            .as_array()
            .expect("rejection list")
            .iter()
            .any(|row| row["instance_id"] == "counter476"),
        "{r:?}"
    );
}

// 平台派生密钥走同一资源写入边界，不能从通用管理接口绕过其 managed 保护。
async fn write_key(cp: &TestCp, doc: Value) -> Result<i64, ApiError> {
    let ctx = WriteCtx::of(cp.state());
    cp.db()
        .tx(move |conn| {
            async move {
                let write = ResourceWrite {
                    kind: Kind::ApiKeys,
                    id: uuid::Uuid::new_v4(),
                    op: WriteOp::Create(&doc),
                    expected_version: None,
                    managed_by: Some("tenant:counter476"),
                };
                Ok(write_resource(conn, &ctx, write).await?.version)
            }
            .boxed()
        })
        .await
}

async fn projected_counter(
    cp: &TestCp,
    admin: &OperatorSession,
    endpoint: &str,
    expected: &Value,
) -> i64 {
    let mut kine = etcd_client::Client::connect([endpoint], None)
        .await
        .expect("real kine");
    let key = format!(
        "{}token_counting/{}",
        cp.env().env_prefix(),
        cp.env().env_id
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let revision = loop {
        let r = kine
            .get(key.clone(), None)
            .await
            .expect("read real projected counter");
        if let Some(kv) = r.kvs().first() {
            let actual: Value = serde_json::from_slice(kv.value()).expect("projected JSON");
            assert_eq!(
                &actual, expected,
                "projection preserves the whole configured mapping"
            );
            break kv.mod_revision();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "counter was never projected"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    loop {
        let r = cp
            .send(
                "GET",
                &format!("/admin/resources/token_counting/{}", cp.env().env_id),
                Some(admin.cookie()),
                None,
            )
            .await;
        assert_eq!(r.status, 200, "{r:?}");
        if r.body["rollout"]["kine_revision"] == revision {
            return revision;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "projection acknowledgement missing: {r:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

// 已验历史能力必须同时作用于保存与应用状态，且 DEV/发行控制面都使用真实投影。
#[tokio::test]
async fn historical_verified_builds_reach_applied_after_real_projection_on_both_counter_surfaces() {
    let Some(endpoint) = support::kine_endpoint() else {
        return;
    };
    for cp_version in [HOTFIX, "1.9.0+sha-a11ce11"] {
        for field in ["messages_count_tokens", "responses_input_tokens"] {
            let Some(cp) = TestCp::start_with(|cfg| {
                cfg.cp_version = cp_version.into();
                cfg.kine_endpoint = endpoint.clone();
            })
            .await
            else {
                return;
            };
            let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
            let doc = settings_for(&cp, &admin, field).await;
            let old = beat(&cp, &admin, OLD).await;
            age(&cp, &old).await;
            assert_eq!(
                retire(&cp, &admin, &old, json!({"confirmed_stopped":true}))
                    .await
                    .status,
                200
            );
            let bundle = cp.issue_bundle(&admin, "capable-rollout476").await;
            let client = cp.dp_client(&bundle);
            for version in ["dev+sha-119796c", "dev+sha-5a2b3bc", CAPABLE] {
                let r = client
                    .heartbeat(
                        json!({"dp_id":bundle.dp_id,"instance_id":"capable476","version":version}),
                    )
                    .await;
                assert_eq!(r.status, 200, "{r:?}");
            }
            assert_eq!(
                create(&cp, &admin, "token_counting", doc.clone())
                    .await
                    .status,
                201
            );
            let revision = projected_counter(&cp, &admin, &endpoint, &doc).await;
            for version in ["dev+sha-119796c", "dev+sha-5a2b3bc", CAPABLE] {
                let r = client.heartbeat(json!({"dp_id":bundle.dp_id,"instance_id":"capable476","version":version,"applied_revision":revision})).await;
                assert_eq!(r.status, 200, "{r:?}");
                let r = cp
                    .send(
                        "GET",
                        &format!("/admin/resources/token_counting/{}", cp.env().env_id),
                        Some(admin.cookie()),
                        None,
                    )
                    .await;
                assert_eq!(r.status, 200, "{r:?}");
                assert_eq!(r.body["rollout"]["state"], "applied", "{r:?}");
                assert_eq!(r.body["rollout"]["online"], 1, "{r:?}");
                assert_eq!(r.body["rollout"]["applied"], 1, "{r:?}");
                assert_eq!(r.body["rollout"]["kine_revision"], revision, "{r:?}");
            }
        }
    }
}

// 退役先获得行锁时，随后已接受的真实心跳仍必须清标，不能依赖提交时间比较。
#[tokio::test]
async fn real_heartbeat_after_a_queued_retirement_clears_the_new_marker() {
    let Some(cp) = TestCp::start_stamped(HOTFIX).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = cp.issue_bundle(&admin, "reverse-race476").await;
    let client = cp.dp_client(&bundle);
    let body = json!({"dp_id":bundle.dp_id,"instance_id":"counter476","version":OLD});
    assert_eq!(client.heartbeat(body.clone()).await.status, 200);
    age(&cp, &bundle.dp_id).await;
    let mut blocker = cp.db().pool().begin().await.expect("lock row");
    sqlx::query("SELECT 1 FROM dp_instances WHERE dp_id=$1::uuid FOR UPDATE")
        .bind(&bundle.dp_id)
        .execute(&mut *blocker)
        .await
        .expect("hold race row");
    let retirement = retire(
        &cp,
        &admin,
        &bundle.dp_id,
        json!({"confirmed_stopped":true}),
    );
    let heartbeat = async {
        wait_for_lock(&cp, "%SELECT last_seen_at > now()%FOR UPDATE%").await;
        client.heartbeat(body).await
    };
    let release = async {
        wait_for_lock(&cp, "%INSERT INTO dp_instances%ON CONFLICT%").await;
        blocker.rollback().await.expect("release row");
    };
    let (retired, beat, ()) = tokio::join!(retirement, heartbeat, release);
    assert_eq!(retired.status, 200, "{retired:?}");
    assert_eq!(beat.status, 200, "{beat:?}");
    assert!(self::retirement(&cp, &bundle.dp_id).await.is_none());
    assert_eq!(audit_count(&cp).await, 1);
}
