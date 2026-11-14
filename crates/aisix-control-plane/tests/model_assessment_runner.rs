//! 双控制面真实数据库边界；不启动模型工作器，也不向模型发送请求。

mod support;

use aisix_control_plane::auth::{OperatorAuth, OperatorRole};
use aisix_control_plane::model_assessments::contracts::*;
use aisix_control_plane::model_assessments::history::{
    self, ClaimedRun, Reservation, ReserveOutcome,
};
use aisix_control_plane::model_assessments::suite::SuitePlan;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use support::{OperatorSession, TestCp};
use uuid::Uuid;

async fn target(cp: &TestCp, admin: &OperatorSession, name: &str) -> Value {
    let key = cp.send("POST", "/admin/resources/provider_keys", Some(admin.cookie()), Some(json!({"doc": {
        "display_name": format!("runner-account-{name}"), "provider":"openai", "api_key":"never-dispatched-component-key", "api_base":"http://127.0.0.1:1/v1", "groups":["shared"]
    }}))).await;
    assert_eq!(key.status, 201, "{key:?}");
    let model = cp.send("POST", "/admin/resources/models", Some(admin.cookie()), Some(json!({"doc": {
        "display_name":format!("runner-model-{name}"), "provider":"openai", "model_name":"component-exact-target", "provider_key_id":key.body["id"]
    }}))).await;
    assert_eq!(model.status, 201, "{model:?}");
    let version = |value: &Value| {
        value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.as_i64().expect("资源版本").to_string())
    };
    json!({"model_id":model.body["id"], "model_version":version(&model.body["version"]), "provider_key_version":version(&key.body["version"])})
}

async fn claim_without_dispatch(cp: &TestCp, admin: &OperatorSession) -> ClaimedRun {
    let reference = target(cp, admin, "reference").await;
    let candidate = target(cp, admin, "candidate").await;
    let planned = cp.send("POST", "/admin/model-assessments/plan", Some(admin.cookie()), Some(json!({
        "reference":reference, "candidate":candidate, "mode":"quick", "suite_id":"capability-v1", "suite_version":"1",
        "reference_source":{"kind":"verified_channel", "note":"independent synthetic component test", "verified_at":"2026-01-01T00:00:00Z", "independence_confirmed":true}
    }))).await;
    assert_eq!(planned.status, 201, "{planned:?}");
    let id: Uuid = serde_json::from_value(planned.body["id"].clone()).expect("计划UUID");
    let owner_generation = Uuid::new_v4();
    // 只测试持久状态机；HTTP start会运行真正Bridge，因此这里不调用start。
    let deadline_at = sqlx::query_scalar("UPDATE model_assessment_runs SET state='running',owner_generation=$2,heartbeat_at=clock_timestamp(),started_at=clock_timestamp(),deadline_at=clock_timestamp()+interval '180 seconds' WHERE id=$1 AND state='planned' RETURNING deadline_at")
        .bind(id).bind(owner_generation).fetch_one(cp.state().db.pool()).await.expect("人工领取测试run");
    let plan: Value = sqlx::query_scalar("SELECT plan FROM model_assessment_runs WHERE id=$1")
        .bind(id)
        .fetch_one(cp.state().db.pool())
        .await
        .expect("冻结计划");
    let plan: SuitePlan = serde_json::from_value(plan).expect("计划DTO");
    ClaimedRun {
        run: history::read(cp.state(), id).await.expect("运行快照"),
        plan,
        owner_generation,
        deadline_at,
    }
}

async fn reserve(cp: &TestCp, claim: &ClaimedRun) -> Reservation {
    match history::reserve_next(cp.state(), claim)
        .await
        .expect("一次预留")
    {
        ReserveOutcome::Reserved(reservation) => reservation,
        _ => panic!("空闲数据库应准入一次请求"),
    }
}

#[tokio::test]
async fn a_second_control_plane_cancels_after_permission_without_fabricating_known_sent() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let claim = claim_without_dispatch(&cp, &admin).await;
    let reservation = reserve(&cp, &claim).await;
    assert!(history::mark_sent(cp.state(), &reservation)
        .await
        .expect("最后发送许可"));
    let other_cp = cp.restart().await;
    let cancelled = other_cp
        .send(
            "POST",
            &format!("/admin/model-assessments/{}/cancel", claim.run.summary.id),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(cancelled.status, 200, "{cancelled:?}");
    assert_eq!(
        history::control(cp.state(), &claim)
            .await
            .expect("跨CP控制"),
        Some(CompletionReason::Cancelled)
    );
    assert!(!history::record_sent(cp.state(), &reservation)
        .await
        .expect("迟到的发送记录被隔离"));
    let run = history::read(cp.state(), claim.run.summary.id)
        .await
        .expect("取消快照");
    assert_eq!(run.summary.sent_requests, 0, "许可不等于已发送");
    assert_eq!(run.summary.reserved_requests, 1, "取消不退潜在收费预算");
    assert_eq!(run.summary.unknown_requests, 1);
    let slot: (Uuid, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) = sqlx::query_as("SELECT sample_id,dispatch_deadline_at,isolated_until FROM model_assessment_admission WHERE sample_id=$1")
        .bind(reservation.sample.id).fetch_one(cp.state().db.pool()).await.expect("固定隔离槽");
    assert_eq!(slot.0, reservation.sample.id);
    assert_eq!(slot.1, reservation.dispatch_deadline_at);
    assert_eq!(
        slot.2,
        reservation.dispatch_deadline_at + chrono::Duration::seconds(60)
    );
    let mut stale = reservation.sample.clone();
    stale.dispatch_state = DispatchState::Finished;
    stale.result = SampleResult::Pass;
    stale.score = Some(1);
    assert!(!history::finish_sample(
        cp.state(),
        &reservation,
        stale,
        Some(NormalizedAnswer {
            text: "42".into(),
            tool_calls: vec![]
        })
    )
    .await
    .expect("旧owner不能改写终态"));
    assert!(matches!(
        history::reserve_next(other_cp.state(), &claim)
            .await
            .expect("取消后的领取"),
        ReserveOutcome::Stopped(CompletionReason::Cancelled)
    ));
}

#[tokio::test]
async fn revision_change_between_reservation_and_permission_fences_the_send() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let claim = claim_without_dispatch(&cp, &admin).await;
    let reservation = reserve(&cp, &claim).await;
    let other_cp = cp.restart().await;
    sqlx::query("UPDATE gw_resources SET version=version+1 WHERE kind='provider_keys' AND id=$1")
        .bind(reservation.target.account_id)
        .execute(other_cp.state().db.pool())
        .await
        .expect("另一CP改变账号revision");
    assert!(!history::mark_sent(cp.state(), &reservation)
        .await
        .expect("最后revision门"));
    let run = history::read(cp.state(), claim.run.summary.id)
        .await
        .expect("停止快照");
    assert_eq!(run.summary.state, RunState::Interrupted);
    assert_eq!(
        run.completion_reason,
        Some(CompletionReason::ConfigurationChanged)
    );
    assert_eq!(run.summary.reserved_requests, 1);
    assert_eq!(run.summary.sent_requests, 0);
    assert_eq!(run.summary.unknown_requests, 1);
    assert!(history::claim_run(other_cp.state(), claim.run.summary.id)
        .await
        .expect("不得重放")
        .is_none());
}

#[derive(Clone, Copy)]
enum Revocation {
    Disabled,
    Operator,
    Viewer,
}

impl Revocation {
    fn body(self) -> Value {
        match self {
            Self::Disabled => json!({"disabled":true}),
            Self::Operator => json!({"role":"operator"}),
            Self::Viewer => json!({"role":"viewer"}),
        }
    }
}

async fn revoke(
    cp: &TestCp,
    other_admin: &OperatorSession,
    starter: &OperatorSession,
    change: Revocation,
) {
    let result = cp
        .send(
            "PATCH",
            &format!("/admin/operators/{}", starter.id),
            Some(other_admin.cookie()),
            Some(change.body()),
        )
        .await;
    assert_eq!(result.status, 200, "真实管理员撤权：{result:?}");
    let fresh = cp
        .send(
            "GET",
            "/admin/model-assessment-suites",
            Some(starter.cookie()),
            None,
        )
        .await;
    assert_eq!(
        fresh.status,
        match change {
            Revocation::Disabled => 401,
            Revocation::Operator | Revocation::Viewer => 403,
        },
        "撤权后新请求必须立即失去权限：{fresh:?}"
    );
}

async fn plan_without_dispatch(cp: &TestCp, admin: &OperatorSession) -> Uuid {
    let reference = target(cp, admin, "reference").await;
    let candidate = target(cp, admin, "candidate").await;
    let planned = cp.send("POST", "/admin/model-assessments/plan", Some(admin.cookie()), Some(json!({
        "reference":reference, "candidate":candidate, "mode":"quick", "suite_id":"capability-v1", "suite_version":"1",
        "reference_source":{"kind":"verified_channel", "note":"independent synthetic component test", "verified_at":"2026-01-01T00:00:00Z", "independence_confirmed":true}
    }))).await;
    assert_eq!(planned.status, 201, "{planned:?}");
    serde_json::from_value(planned.body["id"].clone()).expect("计划UUID")
}

async fn assert_reserved_call_stays_quarantined(
    cp: &TestCp,
    run_id: Uuid,
    reservation: &Reservation,
) {
    let run = history::read(cp.state(), run_id).await.expect("撤权终态");
    assert_eq!(run.summary.state, RunState::Interrupted);
    assert_eq!(
        run.completion_reason,
        Some(CompletionReason::ConfigurationChanged)
    );
    assert_eq!(run.summary.reserved_requests, 1, "撤权不能退还预留预算");
    assert_eq!(run.summary.sent_requests, 0, "许可不等于已发送");
    assert_eq!(run.summary.unknown_requests, 1);
    let slot: (Uuid, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) = sqlx::query_as("SELECT sample_id,dispatch_deadline_at,isolated_until FROM model_assessment_admission WHERE sample_id=$1")
        .bind(reservation.sample.id).fetch_one(cp.state().db.pool()).await.expect("撤权不能提前释放隔离槽");
    assert_eq!(slot.0, reservation.sample.id);
    assert_eq!(slot.1, reservation.dispatch_deadline_at);
    assert_eq!(
        slot.2,
        reservation.dispatch_deadline_at + chrono::Duration::seconds(60)
    );
    assert!(!history::record_sent(cp.state(), reservation)
        .await
        .expect("撤权后的迟到已发送记录被拒绝"));
    assert!(history::claim_run(cp.state(), run_id)
        .await
        .expect("撤权后的作业不得重放")
        .is_none());
}

async fn queued_revocation(change: Revocation) {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let starter = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = plan_without_dispatch(&cp, &starter).await;
    let other_cp = cp.restart().await;
    let other_admin = other_cp.admin_login_seeded(OperatorRole::Admin).await;
    // 不调用HTTP start；先在事务中准备队列，撤权提交后才对工作器可见。
    let mut queued = cp.state().db.pool().begin().await.expect("队列事务");
    sqlx::query("UPDATE model_assessment_runs SET state='queued',started_at=clock_timestamp(),deadline_at=clock_timestamp()+interval '180 seconds' WHERE id=$1 AND state='planned'")
        .bind(id).execute(&mut *queued).await.expect("人工准备尚未预留的queued");
    revoke(&other_cp, &other_admin, &starter, change).await;
    queued.commit().await.expect("提交撤权后的队列");
    assert!(history::claim_run(cp.state(), id)
        .await
        .expect("撤权后不能领取队列")
        .is_none());
    let run = history::read(cp.state(), id).await.expect("撤权队列终态");
    assert_eq!(run.summary.state, RunState::Interrupted);
    assert_eq!(
        run.completion_reason,
        Some(CompletionReason::ConfigurationChanged)
    );
    assert_eq!(run.summary.reserved_requests, 0);
    assert_eq!(run.summary.sent_requests, 0);
    assert_eq!(run.summary.unknown_requests, 0);
    assert!(history::claim_run(other_cp.state(), id)
        .await
        .expect("另一控制面也不能重放")
        .is_none());
}

#[derive(Clone, Copy)]
enum RevocationGate {
    Control,
    Reserve,
    Permission,
}

async fn running_revocation(change: Revocation, gate: RevocationGate) {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let starter = cp.admin_login_seeded(OperatorRole::Admin).await;
    let claim = claim_without_dispatch(&cp, &starter).await;
    let reservation = reserve(&cp, &claim).await;
    let other_cp = cp.restart().await;
    let other_admin = other_cp.admin_login_seeded(OperatorRole::Admin).await;
    revoke(&other_cp, &other_admin, &starter, change).await;
    match gate {
        RevocationGate::Control => {
            assert_eq!(
                history::control(cp.state(), &claim)
                    .await
                    .expect("在途控制检查当前发起人"),
                Some(CompletionReason::ConfigurationChanged)
            );
            // 与真实工作器相同：收到停止理由后持久化终态并保留未知在途。
            history::stop_run(cp.state(), &claim, CompletionReason::ConfigurationChanged)
                .await
                .expect("持久化撤权停止");
        }
        RevocationGate::Reserve => {
            assert!(matches!(
                history::reserve_next(cp.state(), &claim)
                    .await
                    .expect("撤权后拒绝新的预算预留"),
                ReserveOutcome::Stopped(CompletionReason::ConfigurationChanged)
            ));
            history::stop_run(cp.state(), &claim, CompletionReason::ConfigurationChanged)
                .await
                .expect("持久化撤权停止");
        }
        RevocationGate::Permission => {
            assert!(!history::mark_sent(cp.state(), &reservation)
                .await
                .expect("预留之后撤权，最后发送许可必须拒绝"));
        }
    }
    assert_reserved_call_stays_quarantined(&other_cp, claim.run.summary.id, &reservation).await;
    assert!(matches!(
        history::reserve_next(cp.state(), &claim)
            .await
            .expect("撤权终态不追加预留"),
        ReserveOutcome::Stopped(CompletionReason::ConfigurationChanged)
    ));
}

async fn stale_start_authorization(change: Revocation) {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let starter = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = plan_without_dispatch(&cp, &starter).await;
    let extracted_before_revoke = OperatorAuth {
        id: starter.id,
        email: starter.email.clone(),
        role: OperatorRole::Admin,
    };
    let other_cp = cp.restart().await;
    let other_admin = other_cp.admin_login_seeded(OperatorRole::Admin).await;
    revoke(&other_cp, &other_admin, &starter, change).await;
    // 提取器的旧身份可随排队请求保留；开始事务必须再次读取当前权限。
    let error = history::start(
        cp.state(),
        &extracted_before_revoke,
        id,
        StartRequest {
            client_request_id: Uuid::new_v4(),
            accept_unknown_price: true,
        },
    )
    .await
    .expect_err("旧的管理员身份不能开始新的收费工作");
    assert_eq!(
        error.status.as_u16(),
        match change {
            Revocation::Disabled => 401,
            Revocation::Operator | Revocation::Viewer => 403,
        }
    );
    let run = history::read(other_cp.state(), id)
        .await
        .expect("开始拒绝后的计划");
    assert_eq!(run.summary.state, RunState::Planned);
    assert_eq!(run.summary.reserved_requests, 0);
    assert_eq!(run.summary.sent_requests, 0);
}

macro_rules! revocation_tests {
    ($queued:ident, $control:ident, $reserve:ident, $permission:ident, $start:ident, $change:expr) => {
        #[tokio::test]
        async fn $queued() {
            queued_revocation($change).await;
        }
        #[tokio::test]
        async fn $control() {
            running_revocation($change, RevocationGate::Control).await;
        }
        #[tokio::test]
        async fn $reserve() {
            running_revocation($change, RevocationGate::Reserve).await;
        }
        #[tokio::test]
        async fn $permission() {
            running_revocation($change, RevocationGate::Permission).await;
        }
        #[tokio::test]
        async fn $start() {
            stale_start_authorization($change).await;
        }
    };
}

revocation_tests!(
    disabled_starter_cannot_be_claimed,
    disabled_starter_stops_active_control,
    disabled_starter_cannot_reserve,
    disabled_starter_cannot_receive_send_permission,
    disabled_starter_cannot_start_with_stale_authorization,
    Revocation::Disabled
);
revocation_tests!(
    operator_starter_cannot_be_claimed,
    operator_starter_stops_active_control,
    operator_starter_cannot_reserve,
    operator_starter_cannot_receive_send_permission,
    operator_starter_cannot_start_with_stale_authorization,
    Revocation::Operator
);
revocation_tests!(
    viewer_starter_cannot_be_claimed,
    viewer_starter_stops_active_control,
    viewer_starter_cannot_reserve,
    viewer_starter_cannot_receive_send_permission,
    viewer_starter_cannot_start_with_stale_authorization,
    Revocation::Viewer
);

#[tokio::test]
async fn a_live_admin_still_controls_reserves_and_receives_permission() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let claim = claim_without_dispatch(&cp, &admin).await;
    assert_eq!(
        history::control(cp.state(), &claim)
            .await
            .expect("有效控制"),
        None
    );
    let reservation = reserve(&cp, &claim).await;
    assert!(history::mark_sent(cp.state(), &reservation)
        .await
        .expect("有效管理员仍得到许可"));
    let run = history::read(cp.state(), claim.run.summary.id)
        .await
        .expect("有效预留快照");
    assert_eq!(run.summary.state, RunState::Running);
    assert_eq!(run.summary.reserved_requests, 1);
    assert_eq!(run.summary.sent_requests, 0);
}

#[tokio::test]
async fn a_live_admin_can_start_a_plan_after_its_planner_was_disabled() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let planner = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = plan_without_dispatch(&cp, &planner).await;
    let other_cp = cp.restart().await;
    let starter = other_cp.admin_login_seeded(OperatorRole::Admin).await;
    revoke(&other_cp, &starter, &planner, Revocation::Disabled).await;
    let run = history::start(
        cp.state(),
        &OperatorAuth {
            id: starter.id,
            email: starter.email.clone(),
            role: OperatorRole::Admin,
        },
        id,
        StartRequest {
            client_request_id: Uuid::new_v4(),
            accept_unknown_price: true,
        },
    )
    .await
    .expect("当前有效的实际发起人可以开始计划");
    assert_eq!(run.actor_id, starter.id, "工作器必须检查实际发起人");
}

async fn complete_http_errors_release_admission(status: u16) {
    complete_http_errors_release_admission_for_accounts(status, false).await;
}

fn runner_warning_diagnostics() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "off,aisix_control_plane::model_assessments::runner=warn",
            ))
            .with_ansi(false)
            .without_time()
            .with_test_writer()
            .try_init()
            .expect("组件测试必须安装限定runner的诊断订阅器");
    });
}

async fn complete_http_errors_release_admission_for_accounts(status: u16, shared_account: bool) {
    runner_warning_diagnostics();
    let Some(cp) = TestCp::start_reaching_loopback().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(move || {
        let observed = Arc::clone(&observed);
        async move {
            observed.fetch_add(1, Ordering::SeqCst);
            (axum::http::StatusCode::from_u16(status).expect("合法HTTP状态"), axum::Json(json!({
                "error":{"message":"complete component HTTP error", "type":match status {400=>"invalid_request_error",403=>"permission_error",_=>"server_error"}}
            })))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("独立HTTP组件监听");
    let endpoint = format!("http://{}/v1", listener.local_addr().expect("组件端口"));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("HTTP组件服务");
    });
    let mut targets = vec![];
    let mut first_account = Value::Null;
    for name in ["reference", "candidate"] {
        let account = if shared_account && name == "candidate" {
            first_account.clone()
        } else {
            let account = cp.send("POST", "/admin/resources/provider_keys", Some(admin.cookie()), Some(json!({"doc": {
            "display_name":format!("known-http-{name}"), "provider":"openai", "api_key":"component-not-a-real-credential",
            "api_base":endpoint, "groups":["shared"]
        }}))).await;
            assert_eq!(account.status, 201, "{account:?}");
            account.body
        };
        first_account = account.clone();
        let model = cp.send("POST", "/admin/resources/models", Some(admin.cookie()), Some(json!({"doc": {
            "display_name":format!("known-http-{name}"), "provider":"openai", "model_name":"component-exact-model", "provider_key_id":account["id"]
        }}))).await;
        assert_eq!(model.status, 201, "{model:?}");
        let revision = |value: &Value| {
            value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.as_i64().expect("资源版本").to_string())
        };
        targets.push(json!({"model_id":model.body["id"], "model_version":revision(&model.body["version"]), "provider_key_version":revision(&account["version"])}));
    }
    let planned = cp.send("POST", "/admin/model-assessments/plan", Some(admin.cookie()), Some(json!({
        "reference":targets[0], "candidate":targets[1], "mode":"quick", "suite_id":"capability-v1", "suite_version":"1",
        "reference_source":{"kind":"verified_channel", "note":"HTTP error admission component regression only", "verified_at":"2026-01-01T00:00:00Z", "independence_confirmed":true}
    }))).await;
    if shared_account {
        server.abort();
        // 冻结接口要求不同账号；这个新增错误样本不能用于放宽生产合同。
        assert_eq!(planned.status, 422, "{planned:?}");
        assert_eq!(planned.body["code"], "validation_failed");
        assert_eq!(requests.load(Ordering::SeqCst), 0, "非法计划不能调用模型");
        return;
    }
    assert_eq!(planned.status, 201, "{planned:?}");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "计划不能发出模型HTTP请求"
    );
    let id: Uuid = serde_json::from_value(planned.body["id"].clone()).expect("运行UUID");
    let started = cp
        .send(
            "POST",
            &format!("/admin/model-assessments/{id}/start"),
            Some(admin.cookie()),
            Some(json!({
                "client_request_id":Uuid::new_v4(), "accept_unknown_price":true
            })),
        )
        .await;
    assert_eq!(started.status, 202, "{started:?}");
    let finished = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let run = cp
                .send(
                    "GET",
                    &format!("/admin/model-assessments/{id}"),
                    Some(admin.cookie()),
                    None,
                )
                .await;
            assert_eq!(run.status, 200, "{run:?}");
            if matches!(
                run.body["state"].as_str(),
                Some("completed" | "interrupted" | "cancelled")
            ) {
                break run.body;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    server.abort();
    let run = finished.expect("完整HTTP错误不得使两个已终态请求占槽90秒");
    assert_eq!(run["state"], "completed", "{run}");
    assert_eq!(run["completion_reason"], "all_scheduled");
    assert_eq!(run["reserved_requests"], 20, "完成错误不退还预留预算");
    assert_eq!(run["sent_requests"], 20);
    assert_eq!(run["finished_requests"], 20);
    assert_eq!(run["unknown_requests"], 0);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        20,
        "每个样本恰好一次HTTP请求"
    );
    let occupied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM model_assessment_admission WHERE sample_id IS NOT NULL",
    )
    .fetch_one(cp.state().db.pool())
    .await
    .expect("真实数据库准入槽");
    assert_eq!(occupied, 0, "完整终态错误应立即释放准入槽");
    let samples = cp
        .send(
            "GET",
            &format!("/admin/model-assessments/{id}/samples?limit=100"),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(samples.status, 200, "{samples:?}");
    let samples = samples.body["items"].as_array().expect("样本数组");
    assert_eq!(samples.len(), 20);
    for sample in samples {
        assert_eq!(sample["dispatch_state"], "finished");
        assert_eq!(
            sample["result"], "error",
            "HTTP错误不能冒充Pass或Unsupported"
        );
        assert_eq!(sample["http_status"], status);
        assert_eq!(
            sample["error_kind"],
            if status == 403 {
                "auth"
            } else {
                "upstream_http"
            }
        );
        assert_eq!(sample.get("score"), Some(&Value::Null));
        let usage = sample["usage"].as_object().expect("缺量保留显式null对象");
        assert_eq!(usage.len(), 7);
        for field in [
            "input_tokens",
            "output_tokens",
            "upstream_total_tokens",
            "cached_prompt_tokens",
            "cache_read_tokens",
            "cache_creation_tokens",
            "reasoning_tokens",
        ] {
            assert_eq!(
                usage.get(field),
                Some(&Value::Null),
                "缺失上游用量不能变成0或省略字段"
            );
        }
    }
    assert!(history::claim_run(cp.state(), id)
        .await
        .expect("终态不得重放")
        .is_none());
}

#[tokio::test]
async fn twenty_complete_http_400_errors_finish_without_quarantining_known_terminal_calls() {
    complete_http_errors_release_admission(400).await;
}

#[tokio::test]
async fn twenty_complete_http_500_errors_finish_without_quarantining_known_terminal_calls() {
    complete_http_errors_release_admission(500).await;
}

#[tokio::test]
async fn two_models_on_one_account_are_rejected_before_any_http_call() {
    complete_http_errors_release_admission_for_accounts(400, true).await;
}

#[tokio::test]
async fn twenty_complete_http_403_errors_keep_auth_category_and_release_terminal_calls() {
    complete_http_errors_release_admission(403).await;
}

#[tokio::test]
async fn uncertain_transport_timeout_and_incomplete_bodies_keep_the_original_quarantine() {
    for (kind, sent) in [
        (ErrorKind::Transport, false),
        (ErrorKind::Timeout, false),
        (ErrorKind::Decode, true),
        (ErrorKind::Truncated, true),
    ] {
        let Some(cp) = TestCp::start().await else {
            return;
        };
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let claim = claim_without_dispatch(&cp, &admin).await;
        let reservation = reserve(&cp, &claim).await;
        if sent {
            assert!(history::record_sent(cp.state(), &reservation)
                .await
                .expect("上游已应答后断流"));
        }
        let mut sample = reservation.sample.clone();
        sample.dispatch_state = if sent {
            DispatchState::AbortedAfterSend
        } else {
            DispatchState::InterruptedUnknown
        };
        sample.error_kind = Some(kind);
        sample.result = SampleResult::Error;
        sample.score = None;
        assert!(
            history::finish_sample(cp.state(), &reservation, sample, None)
                .await
                .expect("不完整执行持久化")
        );
        let slot:(Uuid,chrono::DateTime<chrono::Utc>,chrono::DateTime<chrono::Utc>)=sqlx::query_as("SELECT sample_id,dispatch_deadline_at,isolated_until FROM model_assessment_admission WHERE sample_id=$1")
            .bind(reservation.sample.id).fetch_one(cp.state().db.pool()).await.expect("未知执行必须保留隔离槽");
        assert_eq!(slot.0, reservation.sample.id);
        assert_eq!(slot.1, reservation.dispatch_deadline_at);
        assert_eq!(
            slot.2,
            reservation.dispatch_deadline_at + chrono::Duration::seconds(60)
        );
        let run = history::read(cp.state(), claim.run.summary.id)
            .await
            .expect("原预算快照");
        assert_eq!(run.summary.reserved_requests, 1);
        assert_eq!(run.summary.sent_requests, u32::from(sent));
        assert!(!history::mark_sent(cp.state(), &reservation)
            .await
            .expect("终态sample不能重发"));
        assert!(history::claim_run(cp.state(), claim.run.summary.id)
            .await
            .expect("已预留作业不能重放")
            .is_none());
    }
}
