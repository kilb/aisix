//! 能力对照的真实鉴权与持久计划边界，不发送任何模型请求。

mod support;

use aisix_control_plane::auth::OperatorRole;
use serde_json::{json, Value};
use support::TestCp;
use uuid::Uuid;

#[tokio::test]
async fn assessment_metadata_and_history_require_admin() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    for path in ["/admin/model-assessment-suites", "/admin/model-assessments"] {
        assert_eq!(cp.send("GET", path, None, None).await.status, 401);
        for role in [OperatorRole::Viewer, OperatorRole::Operator] {
            let caller = cp.admin_login_seeded(role).await;
            assert_eq!(
                cp.send("GET", path, Some(caller.cookie()), None)
                    .await
                    .status,
                403
            );
        }
    }
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let suites = cp
        .send(
            "GET",
            "/admin/model-assessment-suites",
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(suites.status, 200, "{suites:?}");
    assert_eq!(suites.body["items"][0]["id"], "capability-v1");
    let history = cp
        .send(
            "GET",
            "/admin/model-assessments",
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(history.status, 200, "{history:?}");
    assert_eq!(history.body["items"], json!([]));
    assert!(history.body.get("next_cursor").is_some());
    assert_eq!(history.body["next_cursor"], Value::Null);
}

#[tokio::test]
async fn assessment_actions_are_admin_only_and_never_invent_missing_runs() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = Uuid::new_v4();
    for (action, body) in [
        (
            "start",
            json!({"client_request_id":Uuid::new_v4(),"accept_unknown_price":true}),
        ),
        ("cancel", json!({})),
    ] {
        let path = format!("/admin/model-assessments/{id}/{action}");
        assert_eq!(
            cp.send("POST", &path, Some(operator.cookie()), Some(body.clone()))
                .await
                .status,
            403
        );
        assert_eq!(
            cp.send("POST", &path, Some(admin.cookie()), Some(body))
                .await
                .status,
            404
        );
    }
}

#[tokio::test]
async fn assessment_queries_refuse_invalid_bounds_and_cursors() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    for query in ["limit=0", "limit=101", "before=garbage"] {
        let result = cp
            .send(
                "GET",
                &format!("/admin/model-assessments?{query}"),
                Some(admin.cookie()),
                None,
            )
            .await;
        assert_eq!(result.status, 422, "{query}: {result:?}");
    }
}

async fn saved_target(cp: &TestCp, admin: &support::OperatorSession, name: &str) -> Value {
    let account=cp.send("POST","/admin/resources/provider_keys",Some(admin.cookie()),Some(json!({"doc":{"display_name":format!("assessment-account-{name}"),"provider":"openai","api_key":"sk-assessment-secret-never-sent-123456","api_base":"http://user:password@127.0.0.1:1/v1","groups":["shared"]}}))).await;
    assert_eq!(account.status, 201, "{account:?}");
    let model=cp.send("POST","/admin/resources/models",Some(admin.cookie()),Some(json!({"doc":{"display_name":format!("assessment-{name}"),"provider":"openai","model_name":"assessment-concrete","provider_key_id":account.body["id"]}}))).await;
    assert_eq!(model.status, 201, "{model:?}");
    json!({"model_id":model.body["id"],"model_version":model.body["version"].as_i64().map(|n|n.to_string()).or_else(||model.body["version"].as_str().map(str::to_owned)).expect("模型版本"),"provider_key_version":account.body["version"].as_i64().map(|n|n.to_string()).or_else(||account.body["version"].as_str().map(str::to_owned)).expect("账号版本")})
}
async fn plan(
    cp: &TestCp,
    admin: &support::OperatorSession,
    reference: Value,
    candidate: Value,
) -> Value {
    let response=cp.send("POST","/admin/model-assessments/plan",Some(admin.cookie()),Some(json!({"reference":reference,"candidate":candidate,"reference_source":{"kind":"verified_channel","note":"verified https://name:note-password@reference.example.test/","verified_at":"2026-01-01T00:00:00Z","independence_confirmed":true},"mode":"quick","suite_id":"capability-v1","suite_version":"1"}))).await;
    assert_eq!(response.status, 201, "{response:?}");
    response.body
}

#[tokio::test]
async fn planning_and_export_freeze_twenty_unsent_requests_without_fake_usage() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let reference = saved_target(&cp, &admin, "reference").await;
    let candidate = saved_target(&cp, &admin, "candidate").await;
    let run = plan(&cp, &admin, reference, candidate).await;
    assert_eq!(run["state"], "planned");
    assert_eq!(run["planned_requests"], 20);
    assert_eq!(run["reserved_requests"], 0);
    assert!(run.get("estimated_cost").is_some());
    assert_eq!(run["estimated_cost"], Value::Null);
    let export = cp
        .send(
            "GET",
            &format!(
                "/admin/model-assessments/{}/export",
                run["id"].as_str().expect("run id")
            ),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(export.status, 200, "{export:?}");
    assert_eq!(export.body["partial"], true);
    let samples = export.body["samples"].as_array().expect("完整样本数组");
    assert_eq!(samples.len(), 20);
    assert_eq!(
        export.body["tasks"].as_array().expect("冻结合成题").len(),
        10
    );
    for sample in samples {
        assert_eq!(sample["dispatch_state"], "not_sent");
        assert_eq!(sample["result"], "not_run");
        assert!(sample["usage"].get("input_tokens").is_some());
        assert_eq!(sample["usage"]["input_tokens"], Value::Null);
        assert_eq!(sample["normalized_answer"], Value::Null);
        assert!(sample["request_semantic_hash"]
            .as_str()
            .is_some_and(|s| s.len() == 64));
    }
    let serialized = export.body.to_string();
    for secret in [
        "sk-assessment-secret-never-sent",
        "password@",
        "note-password",
    ] {
        assert!(!serialized.contains(secret), "不应保存秘密：{secret}");
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM model_assessment_admission WHERE sample_id IS NOT NULL",
    )
    .fetch_one(cp.state().db.pool())
    .await
    .expect("槽数量");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn a_plan_requires_two_independent_saved_accounts() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let target = saved_target(&cp, &admin, "one").await;
    let response=cp.send("POST","/admin/model-assessments/plan",Some(admin.cookie()),Some(json!({"reference":target,"candidate":target,"reference_source":{"kind":"verified_channel","note":"declared","verified_at":"2026-01-01T00:00:00Z","independence_confirmed":true},"mode":"quick","suite_id":"capability-v1","suite_version":"1"}))).await;
    assert_eq!(response.status, 422, "{response:?}");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM model_assessment_runs")
        .fetch_one(cp.state().db.pool())
        .await
        .expect("run数量");
    assert_eq!(count, 0);
}

use aisix_control_plane::model_assessments::{
    contracts::*,
    history::{self, ClaimedRun, ReserveOutcome},
    suite::SuitePlan,
};

// 这些测试只构造数据库worker所有权，不启动runner，不向任何上游发送请求。
async fn worker_claim(cp: &TestCp, run: Value) -> ClaimedRun {
    let id: Uuid = serde_json::from_value(run["id"].clone()).expect("run id");
    let generation = Uuid::new_v4();
    let deadline:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("UPDATE model_assessment_runs SET state='running',owner_generation=$2,heartbeat_at=clock_timestamp(),started_at=clock_timestamp(),deadline_at=clock_timestamp()+interval '180 seconds' WHERE id=$1 AND state='planned' RETURNING deadline_at").bind(id).bind(generation).fetch_one(cp.state().db.pool()).await.expect("人工worker所有权");
    let document: Value = sqlx::query_scalar("SELECT plan FROM model_assessment_runs WHERE id=$1")
        .bind(id)
        .fetch_one(cp.state().db.pool())
        .await
        .expect("冻结题集");
    let plan: SuitePlan = serde_json::from_value(document).expect("计划DTO");
    ClaimedRun {
        run: history::read(cp.state(), id).await.expect("读取run"),
        plan,
        owner_generation: generation,
        deadline_at: deadline,
    }
}

#[tokio::test]
async fn database_admission_caps_global_requests_and_each_account_without_refunding() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let reference = saved_target(&cp, &admin, "global-reference").await;
    let candidate = saved_target(&cp, &admin, "global-candidate").await;
    let a = worker_claim(
        &cp,
        plan(&cp, &admin, reference.clone(), candidate.clone()).await,
    )
    .await;
    let b = worker_claim(&cp, plan(&cp, &admin, reference, candidate).await).await;
    let (one, two, three, four) = tokio::join!(
        history::reserve_next(cp.state(), &a),
        history::reserve_next(cp.state(), &b),
        history::reserve_next(cp.state(), &a),
        history::reserve_next(cp.state(), &b)
    );
    let outcomes = [one, two, three, four];
    let mut reserved = Vec::new();
    for result in outcomes {
        match result.expect("数据库reservation") {
            ReserveOutcome::Reserved(r) => reserved.push(r),
            ReserveOutcome::Wait => {}
            _ => panic!("不应完成或停止"),
        }
    }
    assert_eq!(reserved.len(), 2, "跨run仅两个全局槽");
    assert_ne!(
        reserved[0].target.account_id, reserved[1].target.account_id,
        "同一账号不并发"
    );
    let counts:(i64,i64)=sqlx::query_as("SELECT count(*),count(DISTINCT account_id) FROM model_assessment_admission WHERE sample_id IS NOT NULL").fetch_one(cp.state().db.pool()).await.expect("真实DB槽");
    assert_eq!(counts, (2, 2));
    let total: i64 =
        sqlx::query_scalar("SELECT sum(reserved_requests)::bigint FROM model_assessment_runs")
            .fetch_one(cp.state().db.pool())
            .await
            .expect("持久预算");
    assert_eq!(total, 2);
    assert!(history::mark_sent(cp.state(), &reserved[0])
        .await
        .expect("最后发送许可"));
    let known: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM model_assessment_samples WHERE sent_at IS NOT NULL",
    )
    .fetch_one(cp.state().db.pool())
    .await
    .expect("known sent");
    assert_eq!(known, 0, "最终许可不冒充已发送");
}

#[tokio::test]
async fn cancellation_fences_reserved_samples_and_quarantines_unknown_dispatches() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let a = saved_target(&cp, &admin, "cancel-reference").await;
    let b = saved_target(&cp, &admin, "cancel-candidate").await;
    let claim = worker_claim(&cp, plan(&cp, &admin, a, b).await).await;
    let reservation = match history::reserve_next(cp.state(), &claim)
        .await
        .expect("reservation")
    {
        ReserveOutcome::Reserved(r) => r,
        _ => panic!("应占槽"),
    };
    let stopped = cp
        .send(
            "POST",
            &format!("/admin/model-assessments/{}/cancel", claim.run.summary.id),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(stopped.status, 200, "{stopped:?}");
    assert_eq!(stopped.body["state"], "cancelled");
    assert_eq!(stopped.body["reserved_requests"], 1);
    assert_eq!(stopped.body["unknown_requests"], 1);
    assert!(!history::mark_sent(cp.state(), &reservation)
        .await
        .expect("取消CAS"));
    assert!(!history::record_sent(cp.state(), &reservation)
        .await
        .expect("迟到发送记录"));
    let mut stale = reservation.sample.clone();
    stale.dispatch_state = DispatchState::Finished;
    stale.result = SampleResult::Pass;
    stale.score = Some(1);
    assert!(
        !history::finish_sample(cp.state(), &reservation, stale, None)
            .await
            .expect("迟到完成CAS")
    );
    assert!(matches!(
        history::reserve_next(cp.state(), &claim)
            .await
            .expect("取消后reservation"),
        ReserveOutcome::Stopped(CompletionReason::Cancelled)
    ));
    let slot:(Uuid,bool)=sqlx::query_as("SELECT sample_id,isolated_until>=dispatch_deadline_at+interval '60 seconds' FROM model_assessment_admission WHERE sample_id=$1").bind(reservation.sample.id).fetch_one(cp.state().db.pool()).await.expect("失联隔离槽");
    assert_eq!(slot, (reservation.sample.id, true));
    let duplicate_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM model_assessment_samples WHERE run_id=$1 AND id<>$2 LIMIT 1",
    )
    .bind(claim.run.summary.id)
    .bind(reservation.sample.id)
    .fetch_one(cp.state().db.pool())
    .await
    .expect("另一样本");
    let duplicate = sqlx::query("UPDATE model_assessment_admission SET run_id=$1,sample_id=$2,account_id=$3,owner_generation=$4,dispatch_deadline_at=$5,isolated_until=$5+interval '60 seconds' WHERE sample_id IS NULL")
        .bind(claim.run.summary.id).bind(duplicate_id).bind(reservation.target.account_id).bind(reservation.owner_generation).bind(reservation.dispatch_deadline_at)
        .execute(cp.state().db.pool()).await.expect_err("数据库账号唯一约束不能被绕过");
    assert_eq!(
        duplicate
            .as_database_error()
            .and_then(|e| e.code())
            .as_deref(),
        Some("23505")
    );
    let again = cp
        .send(
            "POST",
            &format!("/admin/model-assessments/{}/cancel", claim.run.summary.id),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(again.status, 200);
    assert_eq!(again.body["reserved_requests"], 1);
}

#[tokio::test]
async fn resource_changes_stop_new_reservations_and_old_owners_cannot_complete() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let a = saved_target(&cp, &admin, "revision-reference").await;
    let b = saved_target(&cp, &admin, "revision-candidate").await;
    let claim = worker_claim(&cp, plan(&cp, &admin, a, b).await).await;
    sqlx::query("UPDATE gw_resources SET version=version+1 WHERE kind='models' AND id=$1")
        .bind(claim.run.summary.candidate.model_id)
        .execute(cp.state().db.pool())
        .await
        .expect("修改revision");
    assert!(matches!(
        history::reserve_next(cp.state(), &claim)
            .await
            .expect("revision gate"),
        ReserveOutcome::Stopped(CompletionReason::ConfigurationChanged)
    ));
    assert_eq!(
        history::read(cp.state(), claim.run.summary.id)
            .await
            .expect("读取预算")
            .summary
            .reserved_requests,
        0
    );
    history::stop_run(cp.state(), &claim, CompletionReason::ConfigurationChanged)
        .await
        .expect("停止run");
    history::complete_run(cp.state(), &claim)
        .await
        .expect("旧owner完成CAS");
    let run = history::read(cp.state(), claim.run.summary.id)
        .await
        .expect("终态");
    assert_eq!(run.summary.state, RunState::Interrupted);
    assert_eq!(
        run.completion_reason,
        Some(CompletionReason::ConfigurationChanged)
    );
    assert!(run.dimensions.iter().all(|d| d.p_one_sided.is_none()));
}

#[tokio::test]
async fn starts_require_unknown_price_acceptance_and_bind_idempotency_to_the_run() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let a = saved_target(&cp, &admin, "start-reference").await;
    let b = saved_target(&cp, &admin, "start-candidate").await;
    let first = plan(&cp, &admin, a.clone(), b.clone()).await;
    let second = plan(&cp, &admin, a, b).await;
    let id = Uuid::parse_str(first["id"].as_str().expect("run UUID")).expect("UUID");
    let other = Uuid::parse_str(second["id"].as_str().expect("run UUID")).expect("UUID");
    let actor = aisix_control_plane::auth::OperatorAuth {
        id: admin.id,
        email: admin.email.clone(),
        role: OperatorRole::Admin,
    };
    let request_id = Uuid::new_v4();
    let refused = history::start(
        cp.state(),
        &actor,
        id,
        StartRequest {
            client_request_id: request_id,
            accept_unknown_price: false,
        },
    )
    .await
    .expect_err("费用未知不能默许");
    assert_eq!(refused.status.as_u16(), 422);
    let body = StartRequest {
        client_request_id: request_id,
        accept_unknown_price: true,
    };
    let accepted = history::start(cp.state(), &actor, id, body.clone())
        .await
        .expect("接受未知费用");
    assert_eq!(accepted.summary.id, id);
    let replay = history::start(cp.state(), &actor, id, body.clone())
        .await
        .expect("幂等重放");
    assert_eq!(replay.summary.id, id);
    let changed = history::start(cp.state(), &actor, other, body.clone())
        .await
        .expect_err("URL run_id也在payload hash内");
    assert_eq!(changed.status.as_u16(), 409);
    let changed = history::start(
        cp.state(),
        &actor,
        id,
        StartRequest {
            client_request_id: request_id,
            accept_unknown_price: false,
        },
    )
    .await
    .expect_err("body改变冲突");
    assert_eq!(changed.status.as_u16(), 409);
    let new_key = history::start(
        cp.state(),
        &actor,
        id,
        StartRequest {
            client_request_id: Uuid::new_v4(),
            accept_unknown_price: true,
        },
    )
    .await
    .expect_err("同run不能换key重启");
    assert_eq!(new_key.status.as_u16(), 409);
}

#[tokio::test]
async fn expired_plans_and_lost_owners_never_replay_reserved_work() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let a = saved_target(&cp, &admin, "lost-reference").await;
    let b = saved_target(&cp, &admin, "lost-candidate").await;
    let expired = plan(&cp, &admin, a.clone(), b.clone()).await;
    let id = Uuid::parse_str(expired["id"].as_str().expect("id")).expect("UUID");
    sqlx::query("UPDATE model_assessment_runs SET expires_at=clock_timestamp()-interval '1 second',document=jsonb_set(document,'{expires_at}',to_jsonb(clock_timestamp()-interval '1 second')) WHERE id=$1").bind(id).execute(cp.state().db.pool()).await.expect("过期计划");
    let refused = cp
        .send(
            "POST",
            &format!("/admin/model-assessments/{id}/start"),
            Some(admin.cookie()),
            Some(json!({"client_request_id":Uuid::new_v4(),"accept_unknown_price":true})),
        )
        .await;
    assert_eq!(refused.status, 409, "{refused:?}");
    let claim = worker_claim(&cp, plan(&cp, &admin, a, b).await).await;
    let reservation = match history::reserve_next(cp.state(), &claim)
        .await
        .expect("reservation")
    {
        ReserveOutcome::Reserved(r) => r,
        _ => panic!("应预留"),
    };
    sqlx::query("UPDATE model_assessment_runs SET heartbeat_at=clock_timestamp()-interval '20 seconds' WHERE id=$1").bind(claim.run.summary.id).execute(cp.state().db.pool()).await.expect("模拟失联");
    history::maintenance(cp.state()).await.expect("回收失联run");
    assert!(history::claim_run(cp.state(), claim.run.summary.id)
        .await
        .expect("重新领取")
        .is_none());
    assert!(!history::mark_sent(cp.state(), &reservation)
        .await
        .expect("旧owner发送许可"));
    let run = history::read(cp.state(), claim.run.summary.id)
        .await
        .expect("终态");
    assert_eq!(run.summary.state, RunState::Interrupted);
    assert_eq!(run.summary.unknown_requests, 1);
    assert_eq!(run.summary.reserved_requests, 1);
    let state: String =
        sqlx::query_scalar("SELECT dispatch_state FROM model_assessment_samples WHERE id=$1")
            .bind(reservation.sample.id)
            .fetch_one(cp.state().db.pool())
            .await
            .expect("未知样本");
    assert_eq!(state, "interrupted_unknown");
}

#[tokio::test]
async fn concurrent_idempotency_keys_cannot_start_two_distinct_runs() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let a = saved_target(&cp, &admin, "race-reference").await;
    let b = saved_target(&cp, &admin, "race-candidate").await;
    let first = plan(&cp, &admin, a.clone(), b.clone()).await;
    let second = plan(&cp, &admin, a, b).await;
    let one = Uuid::parse_str(first["id"].as_str().expect("id")).expect("UUID");
    let two = Uuid::parse_str(second["id"].as_str().expect("id")).expect("UUID");
    let actor = aisix_control_plane::auth::OperatorAuth {
        id: admin.id,
        email: admin.email.clone(),
        role: OperatorRole::Admin,
    };
    let body = StartRequest {
        client_request_id: Uuid::new_v4(),
        accept_unknown_price: true,
    };
    let (left, right) = tokio::join!(
        history::start(cp.state(), &actor, one, body.clone()),
        history::start(cp.state(), &actor, two, body)
    );
    let results = [left, right];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(results
        .iter()
        .filter_map(|r| r.as_ref().err())
        .all(|e| e.status.as_u16() == 409));
    let started: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM model_assessment_runs WHERE client_request_id IS NOT NULL",
    )
    .fetch_one(cp.state().db.pool())
    .await
    .expect("唯一开始记录");
    assert_eq!(started, 1);
}

#[tokio::test]
async fn budget_and_absolute_deadline_are_checked_before_reservation() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let a = saved_target(&cp, &admin, "budget-reference").await;
    let b = saved_target(&cp, &admin, "budget-candidate").await;
    let mut claim = worker_claim(&cp, plan(&cp, &admin, a, b).await).await;
    sqlx::query("UPDATE model_assessment_runs SET reserved_requests=max_requests WHERE id=$1")
        .bind(claim.run.summary.id)
        .execute(cp.state().db.pool())
        .await
        .expect("永久预留耗尽");
    assert!(matches!(
        history::reserve_next(cp.state(), &claim)
            .await
            .expect("预算检查"),
        ReserveOutcome::Stopped(CompletionReason::Budget)
    ));
    claim.deadline_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    sqlx::query("UPDATE model_assessment_runs SET deadline_at=$2 WHERE id=$1")
        .bind(claim.run.summary.id)
        .bind(claim.deadline_at)
        .execute(cp.state().db.pool())
        .await
        .expect("固定截止过期");
    assert!(matches!(
        history::reserve_next(cp.state(), &claim)
            .await
            .expect("deadline检查"),
        ReserveOutcome::Stopped(CompletionReason::Deadline)
    ));
    let samples:i64=sqlx::query_scalar("SELECT count(*) FROM model_assessment_samples WHERE run_id=$1 AND owner_generation IS NOT NULL").bind(claim.run.summary.id).fetch_one(cp.state().db.pool()).await.expect("不能多发");
    assert_eq!(samples, 0);
}
