//! 持久计划、一次派发和同快照证据；不会自行执行任何模型请求。

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row};
use std::collections::HashSet;
use uuid::Uuid;

use super::{
    admission,
    contracts::*,
    scoring,
    suite::{self, SuitePlan},
    target,
};
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::AppState;

pub struct ClaimedRun {
    pub run: AssessmentRun,
    pub plan: SuitePlan,
    pub owner_generation: Uuid,
    pub deadline_at: DateTime<Utc>,
}
pub struct Reservation {
    pub sample: AssessmentSample,
    pub task: TaskSnapshot,
    pub target: SafeTargetSnapshot,
    pub owner_generation: Uuid,
    pub dispatch_deadline_at: DateTime<Utc>,
}
pub enum ReserveOutcome {
    Reserved(Reservation),
    Wait,
    Done,
    Stopped(CompletionReason),
}

pub(crate) fn invalid(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.into(),
        message: message.into(),
    }])
}
fn encode<T: Serialize>(value: &T) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|_| ApiError::internal("能力对照数据无法保存"))
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| ApiError::internal("能力对照数据无法读取"))
}
fn enum_text<T: Serialize>(value: T) -> Result<String, ApiError> {
    Ok(encode(&value)?
        .as_str()
        .ok_or_else(|| ApiError::internal("能力对照状态无效"))?
        .into())
}
fn from_text<T: DeserializeOwned>(value: &str) -> Result<T, ApiError> {
    decode(Value::String(value.into()))
}

async fn transaction(state: &AppState) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, ApiError> {
    let mut tx = state.db.pool().begin().await.map_err(DbError::from)?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sqlx::query("SET LOCAL lock_timeout='5s'")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    Ok(tx)
}
async fn snapshot(state: &AppState) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, ApiError> {
    let mut tx = state.db.pool().begin().await.map_err(DbError::from)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    Ok(tx)
}
async fn samples_on(conn: &mut PgConnection, id: Uuid) -> Result<Vec<ExportSample>, ApiError> {
    load_samples(conn, id, false).await
}
async fn load_samples(
    conn: &mut PgConnection,
    id: Uuid,
    answers: bool,
) -> Result<Vec<ExportSample>, ApiError> {
    let rows = sqlx::query("SELECT document,CASE WHEN $2 THEN normalized_answer ELSE NULL END AS normalized_answer,dispatch_state,owner_generation,dispatch_deadline_at,sent_at,finished_at FROM model_assessment_samples WHERE run_id=$1 ORDER BY ordinal")
        .bind(id).bind(answers).fetch_all(conn).await.map_err(DbError::from)?;
    rows.into_iter()
        .map(|row| {
            let mut sample: AssessmentSample = decode(row.get("document"))?;
            sample.dispatch_state = from_text(row.get("dispatch_state"))?;
            sample.owner_generation = row.get("owner_generation");
            sample.dispatch_deadline_at = row.get("dispatch_deadline_at");
            sample.sent_at = row.get("sent_at");
            sample.finished_at = row.get("finished_at");
            let answer: Option<Value> = row.get("normalized_answer");
            Ok(ExportSample {
                sample,
                normalized_answer: answer.map(decode).transpose()?,
            })
        })
        .collect()
}
async fn run_on(
    conn: &mut PgConnection,
    id: Uuid,
    locked: bool,
) -> Result<(AssessmentRun, SuitePlan), ApiError> {
    let sql = if locked {
        "SELECT * FROM model_assessment_runs WHERE id=$1 FOR UPDATE"
    } else {
        "SELECT * FROM model_assessment_runs WHERE id=$1"
    };
    let row = sqlx::query(sql)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(DbError::from)?
        .ok_or_else(ApiError::not_found)?;
    let mut run: AssessmentRun = decode(row.get("document"))?;
    let plan: SuitePlan = decode(row.get("plan"))?;
    run.summary.state = from_text(row.get("state"))?;
    run.summary.created_at = row.get("created_at");
    run.actor_id = row.get("actor_id");
    run.expires_at = row.get("expires_at");
    run.summary.started_at = row.get("started_at");
    run.summary.finished_at = row.get("finished_at");
    run.summary.reserved_requests = row.get::<i32, _>("reserved_requests") as u32;
    run.cancel_requested_at = row.get("cancel_requested_at");
    run.completion_reason = row
        .get::<Option<String>, _>("completion_reason")
        .map(|s| from_text(&s))
        .transpose()?;
    Ok((run, plan))
}
fn evaluate(run: &mut AssessmentRun, plan: &SuitePlan, samples: &[ExportSample]) {
    let rows: Vec<_> = samples.iter().map(|s| s.sample.clone()).collect();
    run.summary.sent_requests = rows.iter().filter(|s| s.sent_at.is_some()).count() as u32;
    run.summary.unknown_requests = rows
        .iter()
        .filter(|s| s.dispatch_state == DispatchState::InterruptedUnknown)
        .count() as u32;
    run.summary.finished_requests = rows.iter().filter(|s| s.finished_at.is_some()).count() as u32;
    let context = scoring::RunEvaluationContext {
        mode: run.summary.mode,
        plan: plan.clone(),
        state: run.summary.state,
        completion_reason: run.completion_reason,
        configuration_comparable: !run
            .reason_codes
            .iter()
            .any(|r| r.starts_with("configuration_incomparable")),
        independence_confirmed: run.reference_source.independence_confirmed,
        ever_stopped: run.cancel_requested_at.is_some()
            || (run.completion_reason.is_some()
                && run.completion_reason != Some(CompletionReason::AllScheduled)),
    };
    run.dimensions = scoring::summarize(&context, &rows);
    run.summary.verdict = scoring::overall_verdict(&run.dimensions);
    if rows.iter().any(|s| s.evidence.answer_scrubbed)
        && !run
            .warnings
            .iter()
            .any(|s| s == "answer_scrubbed_not_verbatim_rescorable")
    {
        run.warnings
            .push("answer_scrubbed_not_verbatim_rescorable".into());
    }
}
async fn audit_action(
    conn: &mut PgConnection,
    auth: &OperatorAuth,
    id: Uuid,
    action: &'static str,
) -> Result<(), ApiError> {
    audit::record(
        conn,
        AuditEntry {
            actor: Actor::Operator(auth),
            action,
            target_kind: "model_assessment",
            target_id: Some(id.to_string()),
            tenant_id: None,
            detail: json!({"run_id":id}),
        },
    )
    .await?;
    Ok(())
}

pub async fn create(
    state: &AppState,
    auth: &OperatorAuth,
    request: PlanRequest,
) -> Result<AssessmentRun, ApiError> {
    if request.suite_id != "capability-v1" || request.suite_version != "1" {
        return Err(invalid("/suite_id", "不支持该题集版本"));
    }
    if request.reference.model_id == request.candidate.model_id {
        return Err(invalid(
            "/candidate/model_id",
            "参考与被测必须为不同模型资源",
        ));
    }
    if !request.reference_source.independence_confirmed
        || request.reference_source.note.chars().count() > 256
        || request.reference_source.note.trim().is_empty()
        || request.reference_source.verified_at > Utc::now()
    {
        return Err(invalid(
            "/reference_source",
            "请提供有效的独立参考核验声明，备注最多256字符",
        ));
    }
    let reference = target::resolve(state, &request.reference).await?;
    let candidate = target::resolve(state, &request.candidate).await?;
    if reference.account_id == candidate.account_id {
        return Err(invalid("/candidate", "参考与被测必须使用不同账号"));
    }
    let mut reasons = target::comparability(state, &reference).await?;
    reasons.extend(target::comparability(state, &candidate).await?);
    reasons.sort();
    reasons.dedup();
    let mut source = request.reference_source;
    // 备注不包含自由凭据；共享脱敏器先处理，再隐藏URL userinfo。
    let scrubbed = aisix_gateway::upstream_error_scrub::scrub_upstream_error_text(&source.note);
    source.note = aisix_core::redact_userinfo(&scrubbed).to_string();
    let id = Uuid::new_v4();
    let seed = Uuid::new_v4().to_string();
    let plan = suite::build(request.mode, &seed);
    let limits = request.mode.limits();
    if plan.tasks.len() * 2 != limits.max_requests as usize
        || plan
            .tasks
            .iter()
            .any(|t| t.input_bytes > limits.max_input_bytes_per_request)
    {
        return Err(ApiError::internal("能力对照题集与固定限额不一致"));
    }
    let mut tx = transaction(state).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let run = AssessmentRun {
        summary: AssessmentRunSummary {
            id,
            state: RunState::Planned,
            mode: request.mode,
            reference,
            candidate,
            created_at: now,
            started_at: None,
            finished_at: None,
            reserved_requests: 0,
            sent_requests: 0,
            unknown_requests: 0,
            finished_requests: 0,
            planned_requests: limits.max_requests,
            verdict: Verdict::Insufficient,
        },
        schema_version: 1,
        actor_id: auth.id,
        expires_at: now + Duration::minutes(10),
        suite: plan.metadata.clone(),
        reference_source: source,
        limits,
        estimated_cost: None,
        cost_status: CostStatus::Unknown,
        cancel_requested_at: None,
        completion_reason: None,
        reason_codes: reasons,
        dimensions: vec![],
        warnings: vec![
            "suite_thresholds_not_calibrated".into(),
            "no_model_identity_certification".into(),
            "upstream_defaults_not_deterministic".into(),
            "price_unknown".into(),
        ],
    };
    sqlx::query("INSERT INTO model_assessment_runs(id,actor_id,reference_model_id,candidate_model_id,state,created_at,expires_at,max_requests,document,plan) VALUES($1,$2,$3,$4,'planned',$5,$6,$7,$8,$9)")
        .bind(id).bind(auth.id).bind(run.summary.reference.model_id).bind(run.summary.candidate.model_id).bind(now).bind(run.expires_at).bind(run.limits.max_requests as i32).bind(encode(&run)?).bind(encode(&plan)?)
        .execute(&mut *tx).await.map_err(DbError::from)?;
    for (index, task) in plan.tasks.iter().enumerate() {
        for role in [Role::Reference, Role::Candidate] {
            let target = if role == Role::Reference {
                &run.summary.reference
            } else {
                &run.summary.candidate
            };
            let order_hash =
                Sha256::digest(format!("{}:{}", plan.metadata.seed, task.task.pair_id));
            let candidate_first = order_hash[0] & 1 == 1;
            let ordinal =
                (index * 2 + usize::from((role == Role::Candidate) != candidate_first)) as u32;
            let sample = AssessmentSample {
                id: Uuid::new_v4(),
                ordinal,
                pair_id: task.task.pair_id.clone(),
                cluster_id: task.task.cluster_id.clone(),
                dimension: task.dimension,
                role,
                request_semantic_hash: task.request_semantic_hash.clone(),
                task_hash: task.task.task_hash.clone(),
                input_bytes: task.input_bytes,
                dispatch_state: DispatchState::NotSent,
                result: SampleResult::NotRun,
                error_kind: None,
                owner_generation: None,
                dispatch_deadline_at: None,
                sent_at: None,
                finished_at: None,
                http_status: None,
                finish_reason: None,
                score: None,
                usage: Usage::default(),
                duration_ms: None,
                output_bytes: None,
                evidence: Evidence {
                    checks: vec![],
                    expected_hash: hex::encode(Sha256::digest(
                        serde_json::to_vec(&task.task.expected)
                            .map_err(|_| ApiError::internal("题目无法保存"))?,
                    )),
                    answer_preview: None,
                    answer_scrubbed: false,
                },
                effective_request: EffectiveRequest {
                    native_protocol: target.adapter.clone(),
                    adapter_build: env!("CARGO_PKG_VERSION").into(),
                    stream: true,
                    output_cap: run.limits.max_output_tokens_per_request,
                    sampling_policy: "upstream_defaults".into(),
                    semantic_configuration_hash: task.request_semantic_hash.clone(),
                },
                error: None,
            };
            let pair = Uuid::parse_str(&sample.pair_id)
                .map_err(|_| ApiError::internal("题目配对标识无效"))?;
            sqlx::query("INSERT INTO model_assessment_samples(id,run_id,ordinal,pair_id,role,account_id,document) VALUES($1,$2,$3,$4,$5,$6,$7)")
                .bind(sample.id).bind(id).bind(ordinal as i32).bind(pair).bind(role.as_str()).bind(target.account_id).bind(encode(&sample)?).execute(&mut *tx).await.map_err(DbError::from)?;
        }
    }
    audit_action(&mut tx, auth, id, "model_assessment.plan").await?;
    tx.commit().await.map_err(DbError::from)?;
    read(state, id).await
}

async fn replay(
    conn: &mut PgConnection,
    actor: Uuid,
    request: Uuid,
    hash: &str,
) -> Result<Option<Uuid>, ApiError> {
    let row:Option<(Uuid,String)>=sqlx::query_as("SELECT id,start_payload_hash FROM model_assessment_runs WHERE request_actor_id=$1 AND client_request_id=$2")
        .bind(actor).bind(request).fetch_optional(conn).await.map_err(DbError::from)?;
    match row {
        Some((id, old)) if old == hash => Ok(Some(id)),
        Some(_) => Err(ApiError::conflict(
            "idempotency_conflict",
            "该请求标识已用于不同的开始请求",
        )),
        None => Ok(None),
    }
}

async fn current_actor_role(
    conn: &mut PgConnection,
    actor: Uuid,
) -> Result<Option<OperatorRole>, ApiError> {
    // 同时需要run锁的路径先锁run再锁actor；不反向取得运营端的全局锁。
    // FOR SHARE与运营端的角色/禁用UPDATE串行，权限保留到本事务提交。
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM operators WHERE id=$1 AND NOT disabled FOR SHARE")
            .bind(actor)
            .fetch_optional(conn)
            .await
            .map_err(DbError::from)?;
    role.map(|value| value.parse().map_err(ApiError::internal))
        .transpose()
}

async fn require_current_admin(conn: &mut PgConnection, actor: Uuid) -> Result<(), ApiError> {
    match current_actor_role(conn, actor).await? {
        Some(OperatorRole::Admin) => Ok(()),
        Some(_) => Err(ApiError::forbidden()),
        None => Err(ApiError::unauthenticated()),
    }
}

pub async fn start(
    state: &AppState,
    auth: &OperatorAuth,
    id: Uuid,
    body: StartRequest,
) -> Result<AssessmentRun, ApiError> {
    let hash = hex::encode(Sha256::digest(
        serde_json::to_vec(&(id, &body)).map_err(|_| ApiError::internal("开始请求无法保存"))?,
    ));
    let mut tx = transaction(state).await?;
    if let Some(existing) = replay(&mut tx, auth.id, body.client_request_id, &hash).await? {
        require_current_admin(&mut tx, auth.id).await?;
        tx.commit().await.map_err(DbError::from)?;
        return read(state, existing).await;
    }
    let (run, _) = run_on(&mut tx, id, true).await?;
    require_current_admin(&mut tx, auth.id).await?;
    // 同一个run只能接受一次开始；不同client_request_id也不能追加调用。
    if run.summary.state != RunState::Planned {
        if let Some(existing) = replay(&mut tx, auth.id, body.client_request_id, &hash).await? {
            tx.commit().await.map_err(DbError::from)?;
            return read(state, existing).await;
        }
        return Err(ApiError::conflict(
            "already_started",
            "该计划已开始或已终止，请另建计划",
        ));
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    if now >= run.expires_at {
        return Err(ApiError::conflict("plan_expired", "计划已过期，请重新创建"));
    }
    target::verify_revisions(state, &run.summary.reference)
        .await
        .map_err(|_| {
            ApiError::conflict("configuration_changed", "参考配置已改变，请重新创建计划")
        })?;
    target::verify_revisions(state, &run.summary.candidate)
        .await
        .map_err(|_| {
            ApiError::conflict("configuration_changed", "被测配置已改变，请重新创建计划")
        })?;
    if run.estimated_cost.is_none() && !body.accept_unknown_price {
        return Err(invalid(
            "/accept_unknown_price",
            "费用未知，开始前必须明确接受",
        ));
    }
    let result=sqlx::query("UPDATE model_assessment_runs SET state='queued',client_request_id=$2,start_payload_hash=$3,request_actor_id=$4,actor_id=$4,started_at=$5,deadline_at=$5+make_interval(secs=>$6) WHERE id=$1 AND state='planned'")
        .bind(id).bind(body.client_request_id).bind(&hash).bind(auth.id).bind(now).bind(run.limits.run_timeout_seconds as f64).execute(&mut *tx).await;
    match result {
        Ok(_) => {}
        Err(sqlx::Error::Database(ref e)) if e.code().as_deref() == Some("23505") => {
            tx.rollback().await.map_err(DbError::from)?;
            let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
            require_current_admin(&mut conn, auth.id).await?;
            let existing = replay(&mut conn, auth.id, body.client_request_id, &hash)
                .await?
                .ok_or_else(|| {
                    ApiError::conflict("idempotency_conflict", "开始请求与现有记录冲突")
                })?;
            return read(state, existing).await;
        }
        Err(e) => return Err(DbError::from(e).into()),
    }
    audit_action(&mut tx, auth, id, "model_assessment.start").await?;
    tx.commit().await.map_err(DbError::from)?;
    read(state, id).await
}

pub async fn read(state: &AppState, id: Uuid) -> Result<AssessmentRun, ApiError> {
    let mut tx = snapshot(state).await?;
    let (mut run, plan) = run_on(&mut tx, id, false).await?;
    let samples = samples_on(&mut tx, id).await?;
    evaluate(&mut run, &plan, &samples);
    tx.commit().await.map_err(DbError::from)?;
    Ok(run)
}
#[derive(Serialize, Deserialize)]
struct HistoryCursor {
    created_at: DateTime<Utc>,
    id: Uuid,
}
fn cursor<T: Serialize>(value: &T) -> Result<String, ApiError> {
    Ok(URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(value).map_err(|_| ApiError::internal("游标无法保存"))?))
}
fn parse_cursor<T: DeserializeOwned>(value: &str) -> Result<T, ApiError> {
    if value.len() > 256 {
        return Err(invalid("/cursor", "分页游标无效"));
    }
    let raw = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| invalid("/cursor", "分页游标无效"))?;
    serde_json::from_slice(&raw).map_err(|_| invalid("/cursor", "分页游标无效"))
}
pub async fn list(
    state: &AppState,
    model: Option<Uuid>,
    limit: u32,
    before: Option<&str>,
) -> Result<Value, ApiError> {
    let position: Option<HistoryCursor> = before.map(parse_cursor).transpose()?;
    let mut tx = snapshot(state).await?;
    let rows:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM model_assessment_runs WHERE ($1::uuid IS NULL OR reference_model_id=$1 OR candidate_model_id=$1) AND ($2::timestamptz IS NULL OR (created_at,id)<($2,$3)) ORDER BY created_at DESC,id DESC LIMIT $4")
        .bind(model).bind(position.as_ref().map(|p|p.created_at)).bind(position.as_ref().map(|p|p.id)).bind(limit as i64+1).fetch_all(&mut *tx).await.map_err(DbError::from)?;
    let more = rows.len() > limit as usize;
    let mut items = Vec::new();
    for id in rows.into_iter().take(limit as usize) {
        let (mut run, plan) = run_on(&mut tx, id, false).await?;
        let samples = samples_on(&mut tx, id).await?;
        evaluate(&mut run, &plan, &samples);
        items.push(run.summary);
    }
    let next = if more {
        items
            .last()
            .map(|r| {
                cursor(&HistoryCursor {
                    created_at: r.created_at,
                    id: r.id,
                })
            })
            .transpose()?
    } else {
        None
    };
    tx.commit().await.map_err(DbError::from)?;
    Ok(json!({"items":items,"next_cursor":next}))
}
pub async fn samples(
    state: &AppState,
    id: Uuid,
    limit: u32,
    after: Option<&str>,
) -> Result<Value, ApiError> {
    let position: Option<(Uuid, u32)> = after.map(parse_cursor).transpose()?;
    if position.is_some_and(|(run, _)| run != id) {
        return Err(invalid("/after", "游标不属于该对照"));
    }
    let mut tx = snapshot(state).await?;
    run_on(&mut tx, id, false).await?;
    let all = samples_on(&mut tx, id).await?;
    let mut rows: Vec<AssessmentSample> = all
        .into_iter()
        .map(|e| e.sample)
        .filter(|s| position.is_none_or(|(_, n)| s.ordinal > n))
        .collect();
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more {
        rows.last().map(|s| cursor(&(id, s.ordinal))).transpose()?
    } else {
        None
    };
    tx.commit().await.map_err(DbError::from)?;
    Ok(json!({"items":rows,"next_cursor":next}))
}
pub async fn export(state: &AppState, id: Uuid) -> Result<AssessmentExport, ApiError> {
    let mut tx = snapshot(state).await?;
    let (mut run, plan) = run_on(&mut tx, id, false).await?;
    let samples = load_samples(&mut tx, id, true).await?;
    evaluate(&mut run, &plan, &samples);
    let at: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    let export = AssessmentExport {
        schema_version: 1,
        interface_version: "1.1".into(),
        snapshot_at: at,
        partial: !run.summary.state.terminal(),
        run,
        tasks: plan.tasks.into_iter().map(|t| t.task).collect(),
        samples,
    };
    let bytes = serde_json::to_vec(&export).map_err(|_| ApiError::internal("证据无法导出"))?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err(invalid(
            "/export",
            "evidence_limit：导出超过32MiB，未截断证据",
        ));
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(export)
}

pub async fn claim_run(state: &AppState, id: Uuid) -> Result<Option<ClaimedRun>, ApiError> {
    let mut tx = transaction(state).await?;
    let (run, plan) = run_on(&mut tx, id, true).await?;
    if run.summary.state != RunState::Queued || run.summary.reserved_requests != 0 {
        return Ok(None);
    }
    if current_actor_role(&mut tx, run.actor_id).await? != Some(OperatorRole::Admin) {
        stop_on(&mut tx, id, CompletionReason::ConfigurationChanged, None).await?;
        tx.commit().await.map_err(DbError::from)?;
        return Ok(None);
    }
    let generation = Uuid::new_v4();
    let deadline:Option<DateTime<Utc>>=sqlx::query_scalar("UPDATE model_assessment_runs SET state='running',owner_generation=$2,heartbeat_at=clock_timestamp() WHERE id=$1 AND state='queued' AND owner_generation IS NULL AND reserved_requests=0 RETURNING deadline_at")
        .bind(id).bind(generation).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    tx.commit().await.map_err(DbError::from)?;
    Ok(Some(ClaimedRun {
        run: read(state, id).await?,
        plan,
        owner_generation: generation,
        deadline_at: deadline,
    }))
}
pub async fn heartbeat(state: &AppState, run: &ClaimedRun) -> Result<bool, ApiError> {
    let result=sqlx::query("UPDATE model_assessment_runs SET heartbeat_at=clock_timestamp() WHERE id=$1 AND owner_generation=$2 AND state='running' AND cancel_requested_at IS NULL AND deadline_at>clock_timestamp()")
        .bind(run.run.summary.id).bind(run.owner_generation).execute(state.db.pool()).await.map_err(DbError::from)?;
    Ok(result.rows_affected() == 1)
}
pub async fn control(
    state: &AppState,
    run: &ClaimedRun,
) -> Result<Option<CompletionReason>, ApiError> {
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let row=sqlx::query("SELECT actor_id,state,owner_generation,cancel_requested_at,deadline_at<=clock_timestamp() AS expired,completion_reason FROM model_assessment_runs WHERE id=$1")
        .bind(run.run.summary.id).fetch_optional(&mut *conn).await.map_err(DbError::from)?;
    let Some(row) = row else {
        return Ok(Some(CompletionReason::OwnerLost));
    };
    if row.get::<Option<Uuid>, _>("owner_generation") != Some(run.owner_generation) {
        return Ok(Some(CompletionReason::OwnerLost));
    }
    if row
        .get::<Option<DateTime<Utc>>, _>("cancel_requested_at")
        .is_some()
    {
        return Ok(Some(CompletionReason::Cancelled));
    }
    if row.get::<bool, _>("expired") {
        return Ok(Some(CompletionReason::Deadline));
    }
    if row.get::<&str, _>("state") != "running" {
        return Ok(Some(
            row.get::<Option<String>, _>("completion_reason")
                .map(|s| from_text(&s))
                .transpose()?
                .unwrap_or(CompletionReason::OwnerLost),
        ));
    }
    if current_actor_role(&mut conn, row.get("actor_id")).await? != Some(OperatorRole::Admin) {
        return Ok(Some(CompletionReason::ConfigurationChanged));
    }
    Ok(None)
}
async fn current_revisions(conn: &mut PgConnection, run: &AssessmentRun) -> Result<bool, ApiError> {
    for target in [&run.summary.reference, &run.summary.candidate] {
        let versions:Option<(i64,i64)>=sqlx::query_as("SELECT m.version,p.version FROM gw_resources m JOIN gw_resources p ON p.kind='provider_keys' AND p.id=$2 AND NOT p.deleted WHERE m.kind='models' AND m.id=$1 AND NOT m.deleted")
            .bind(target.model_id).bind(target.account_id).fetch_optional(&mut *conn).await.map_err(DbError::from)?;
        if !versions.is_some_and(|(m, p)| {
            m.to_string() == target.model_version && p.to_string() == target.provider_key_version
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}
pub async fn reserve_next(
    state: &AppState,
    claim: &ClaimedRun,
) -> Result<ReserveOutcome, ApiError> {
    let mut tx = transaction(state).await?;
    admission::lock(&mut tx).await?;
    let (run, plan) = run_on(&mut tx, claim.run.summary.id, true).await?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_generation FROM model_assessment_runs WHERE id=$1")
            .bind(run.summary.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if owner != Some(claim.owner_generation) {
        return Ok(ReserveOutcome::Stopped(CompletionReason::OwnerLost));
    }
    if run.summary.state != RunState::Running || run.cancel_requested_at.is_some() {
        return Ok(ReserveOutcome::Stopped(
            run.completion_reason.unwrap_or(CompletionReason::Cancelled),
        ));
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    if now >= claim.deadline_at {
        return Ok(ReserveOutcome::Stopped(CompletionReason::Deadline));
    }
    if current_actor_role(&mut tx, run.actor_id).await? != Some(OperatorRole::Admin)
        || !current_revisions(&mut tx, &run).await?
    {
        return Ok(ReserveOutcome::Stopped(
            CompletionReason::ConfigurationChanged,
        ));
    }
    let all = samples_on(&mut tx, run.summary.id).await?;
    let unsent: Vec<_> = all
        .iter()
        .filter(|e| e.sample.dispatch_state == DispatchState::NotSent)
        .collect();
    if unsent.is_empty() {
        return Ok(
            if all.iter().all(|e| {
                !matches!(
                    e.sample.dispatch_state,
                    DispatchState::Reserved | DispatchState::Sent
                )
            }) {
                ReserveOutcome::Done
            } else {
                ReserveOutcome::Wait
            },
        );
    }
    if run.summary.reserved_requests >= run.limits.max_requests {
        return Ok(ReserveOutcome::Stopped(CompletionReason::Budget));
    }
    let deadline = claim.deadline_at.min(now + Duration::seconds(30));
    let mut attempted_accounts = HashSet::new();
    for item in unsent {
        let target = if item.sample.role == Role::Reference {
            run.summary.reference.clone()
        } else {
            run.summary.candidate.clone()
        };
        // 同账号本轮最多尝试一次；保留其最早ordinal，下一轮仍会重新检查。
        if !attempted_accounts.insert(target.account_id) {
            continue;
        }
        let mut sample = item.sample.clone();
        if !admission::reserve(
            &mut tx,
            run.summary.id,
            sample.id,
            target.account_id,
            claim.owner_generation,
            deadline,
        )
        .await?
        {
            continue;
        }
        let affected=sqlx::query("UPDATE model_assessment_samples SET dispatch_state='reserved',owner_generation=$2,dispatch_deadline_at=$3 WHERE id=$1 AND dispatch_state='not_sent' AND owner_generation IS NULL")
            .bind(sample.id).bind(claim.owner_generation).bind(deadline).execute(&mut *tx).await.map_err(DbError::from)?.rows_affected();
        if affected != 1 {
            return Err(ApiError::internal("样本预留状态冲突"));
        }
        sqlx::query("UPDATE model_assessment_runs SET reserved_requests=reserved_requests+1 WHERE id=$1 AND reserved_requests<max_requests")
            .bind(run.summary.id).execute(&mut *tx).await.map_err(DbError::from)?;
        let task = plan
            .tasks
            .iter()
            .find(|t| t.task.pair_id == sample.pair_id)
            .cloned()
            .ok_or_else(|| ApiError::internal("样本没有冻结题目"))?;
        sample.dispatch_state = DispatchState::Reserved;
        sample.owner_generation = Some(claim.owner_generation);
        sample.dispatch_deadline_at = Some(deadline);
        tx.commit().await.map_err(DbError::from)?;
        return Ok(ReserveOutcome::Reserved(Reservation {
            sample,
            task,
            target,
            owner_generation: claim.owner_generation,
            dispatch_deadline_at: deadline,
        }));
    }
    Ok(ReserveOutcome::Wait)
}
pub async fn mark_sent(state: &AppState, reservation: &Reservation) -> Result<bool, ApiError> {
    let mut tx = transaction(state).await?;
    admission::lock(&mut tx).await?;
    let row=sqlx::query("SELECT r.id FROM model_assessment_runs r JOIN model_assessment_samples s ON s.run_id=r.id WHERE s.id=$1 AND r.owner_generation=$2 AND s.owner_generation=$2 AND r.state='running' AND r.cancel_requested_at IS NULL AND r.deadline_at>clock_timestamp() AND s.dispatch_state='reserved' AND s.dispatch_deadline_at>clock_timestamp() FOR UPDATE OF r")
        .bind(reservation.sample.id).bind(reservation.owner_generation).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let Some(row) = row else {
        return Ok(false);
    };
    let id: Uuid = row.get("id");
    let (run, _) = run_on(&mut tx, id, false).await?;
    if current_actor_role(&mut tx, run.actor_id).await? != Some(OperatorRole::Admin)
        || !current_revisions(&mut tx, &run).await?
    {
        stop_on(
            &mut tx,
            id,
            CompletionReason::ConfigurationChanged,
            Some(reservation.owner_generation),
        )
        .await?;
        tx.commit().await.map_err(DbError::from)?;
        return Ok(false);
    }
    // 这只是最后一次数据库许可；网络发送与数据库提交之间没有分布式原子性。
    // 在收到上游回应之前，预留仍可能收费，但不能声称已经known-sent。
    tx.commit().await.map_err(DbError::from)?;
    Ok(true)
}
pub async fn record_sent(state: &AppState, reservation: &Reservation) -> Result<bool, ApiError> {
    let mut tx = transaction(state).await?;
    admission::lock(&mut tx).await?;
    let row=sqlx::query("SELECT r.id FROM model_assessment_runs r JOIN model_assessment_samples s ON s.run_id=r.id WHERE s.id=$1 AND r.owner_generation=$2 AND s.owner_generation=$2 AND r.state='running' AND s.dispatch_state='reserved' FOR UPDATE OF r")
        .bind(reservation.sample.id).bind(reservation.owner_generation).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    if row.is_none() {
        return Ok(false);
    }
    let affected=sqlx::query("UPDATE model_assessment_samples SET dispatch_state='sent',sent_at=clock_timestamp() WHERE id=$1 AND owner_generation=$2 AND dispatch_state='reserved'")
        .bind(reservation.sample.id).bind(reservation.owner_generation).execute(&mut *tx).await.map_err(DbError::from)?.rows_affected();
    tx.commit().await.map_err(DbError::from)?;
    Ok(affected == 1)
}

pub async fn finish_sample(
    state: &AppState,
    reservation: &Reservation,
    mut sample: AssessmentSample,
    answer: Option<NormalizedAnswer>,
) -> Result<bool, ApiError> {
    // 此入口只接收allowlist DTO；不能把原始请求或完整账号写入样本。
    if sample.id != reservation.sample.id || sample.task_hash != reservation.sample.task_hash {
        return Err(ApiError::internal("样本完成标识不一致"));
    }
    if answer
        .as_ref()
        .is_some_and(|a| serde_json::to_vec(a).map_or(true, |v| v.len() > 65536))
    {
        return Err(invalid(
            "/evidence",
            "evidence_limit：完整规范答案超过64KiB",
        ));
    }
    if sample
        .evidence
        .answer_preview
        .as_ref()
        .is_some_and(|p| p.len() > 1024)
    {
        return Err(invalid("/evidence", "evidence_limit：答案预览超过1024字节"));
    }
    let mut tx = transaction(state).await?;
    admission::lock(&mut tx).await?;
    let row=sqlx::query("SELECT r.id,s.sent_at FROM model_assessment_runs r JOIN model_assessment_samples s ON s.run_id=r.id WHERE s.id=$1 AND r.owner_generation=$2 AND s.owner_generation=$2 AND r.state='running' AND s.dispatch_state IN ('reserved','sent') FOR UPDATE OF r,s")
        .bind(sample.id).bind(reservation.owner_generation).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
    let Some(row) = row else {
        return Ok(false);
    };
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    sample.owner_generation = Some(reservation.owner_generation);
    sample.dispatch_deadline_at = Some(reservation.dispatch_deadline_at);
    sample.sent_at = row.get("sent_at");
    sample.finished_at = Some(now);
    let uncertain = matches!(
        sample.dispatch_state,
        DispatchState::AbortedAfterSend | DispatchState::InterruptedUnknown
    ) || matches!(
        sample.error_kind,
        Some(ErrorKind::Timeout | ErrorKind::Transport)
    );
    if !matches!(
        sample.dispatch_state,
        DispatchState::Finished
            | DispatchState::CancelledBeforeSend
            | DispatchState::AbortedAfterSend
            | DispatchState::InterruptedUnknown
    ) {
        return Err(ApiError::internal("样本完成状态无效"));
    }
    sqlx::query("UPDATE model_assessment_samples SET dispatch_state=$3,finished_at=$4,document=$5,normalized_answer=$6 WHERE id=$1 AND owner_generation=$2 AND dispatch_state IN ('reserved','sent')")
        .bind(sample.id).bind(reservation.owner_generation).bind(enum_text(sample.dispatch_state)?).bind(now).bind(encode(&sample)?).bind(answer.as_ref().map(encode).transpose()?).execute(&mut *tx).await.map_err(DbError::from)?;
    if !uncertain {
        admission::release(&mut tx, sample.id, reservation.owner_generation).await?;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(true)
}

async fn stop_on(
    conn: &mut PgConnection,
    id: Uuid,
    reason: CompletionReason,
    generation: Option<Uuid>,
) -> Result<(), ApiError> {
    let (run, _) = run_on(conn, id, true).await?;
    if run.summary.state.terminal() {
        return Ok(());
    }
    if let Some(generation) = generation {
        let actual: Option<Uuid> =
            sqlx::query_scalar("SELECT owner_generation FROM model_assessment_runs WHERE id=$1")
                .bind(id)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
        if actual != Some(generation) {
            return Ok(());
        }
    }
    let state = if reason == CompletionReason::Cancelled {
        "cancelled"
    } else {
        "interrupted"
    };
    sqlx::query("UPDATE model_assessment_runs SET state=$2,finished_at=clock_timestamp(),completion_reason=$3,cancel_requested_at=CASE WHEN $3='cancelled' THEN COALESCE(cancel_requested_at,clock_timestamp()) ELSE cancel_requested_at END WHERE id=$1")
        .bind(id).bind(state).bind(enum_text(reason)?).execute(&mut *conn).await.map_err(DbError::from)?;
    let rows = samples_on(conn, id).await?;
    for item in rows {
        let mut s = item.sample;
        if !matches!(
            s.dispatch_state,
            DispatchState::NotSent | DispatchState::Reserved | DispatchState::Sent
        ) {
            continue;
        }
        let was = s.dispatch_state;
        s.dispatch_state = match (was, reason) {
            (DispatchState::NotSent, _) => DispatchState::CancelledBeforeSend,
            (DispatchState::Sent, CompletionReason::Cancelled) => DispatchState::AbortedAfterSend,
            _ => DispatchState::InterruptedUnknown,
        };
        s.result = SampleResult::NotRun;
        s.score = None;
        s.finished_at = Some(Utc::now());
        s.error_kind = match reason {
            CompletionReason::Cancelled => Some(ErrorKind::Cancelled),
            CompletionReason::ConfigurationChanged => Some(ErrorKind::ConfigurationChanged),
            _ => None,
        };
        sqlx::query("UPDATE model_assessment_samples SET dispatch_state=$2,finished_at=clock_timestamp(),document=$3 WHERE id=$1")
            .bind(s.id).bind(enum_text(s.dispatch_state)?).bind(encode(&s)?).execute(&mut *conn).await.map_err(DbError::from)?;
    }
    Ok(())
}
pub async fn stop_run(
    state: &AppState,
    claim: &ClaimedRun,
    reason: CompletionReason,
) -> Result<(), ApiError> {
    let mut tx = transaction(state).await?;
    admission::lock(&mut tx).await?;
    stop_on(
        &mut tx,
        claim.run.summary.id,
        reason,
        Some(claim.owner_generation),
    )
    .await?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn complete_run(state: &AppState, claim: &ClaimedRun) -> Result<(), ApiError> {
    let mut tx = transaction(state).await?;
    admission::lock(&mut tx).await?;
    run_on(&mut tx, claim.run.summary.id, true).await?;
    sqlx::query("UPDATE model_assessment_runs r SET state='completed',completion_reason='all_scheduled',finished_at=clock_timestamp() WHERE id=$1 AND owner_generation=$2 AND state='running' AND cancel_requested_at IS NULL AND NOT EXISTS(SELECT 1 FROM model_assessment_samples s WHERE s.run_id=r.id AND s.dispatch_state IN ('not_sent','reserved','sent'))")
        .bind(claim.run.summary.id).bind(claim.owner_generation).execute(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
pub async fn cancel(
    state: &AppState,
    auth: &OperatorAuth,
    id: Uuid,
) -> Result<AssessmentRun, ApiError> {
    let mut tx = transaction(state).await?;
    admission::lock(&mut tx).await?;
    let (run, _) = run_on(&mut tx, id, true).await?;
    if !run.summary.state.terminal() {
        stop_on(&mut tx, id, CompletionReason::Cancelled, None).await?;
        audit_action(&mut tx, auth, id, "model_assessment.cancel").await?;
    }
    tx.commit().await.map_err(DbError::from)?;
    read(state, id).await
}

pub async fn maintenance(state: &AppState) -> Result<Vec<Uuid>, ApiError> {
    let mut tx = transaction(state).await?;
    admission::lock(&mut tx).await?;
    let lost:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM model_assessment_runs WHERE state='running' AND heartbeat_at<clock_timestamp()-interval '10 seconds' ORDER BY id FOR UPDATE")
        .fetch_all(&mut *tx).await.map_err(DbError::from)?;
    for id in lost {
        stop_on(&mut tx, id, CompletionReason::OwnerLost, None).await?;
    }
    sqlx::query("DELETE FROM model_assessment_runs r WHERE ((state='planned' AND created_at<clock_timestamp()-interval '24 hours') OR (state IN ('completed','cancelled','interrupted') AND finished_at<clock_timestamp()-interval '30 days')) AND NOT EXISTS(SELECT 1 FROM model_assessment_admission a WHERE a.run_id=r.id)")
        .execute(&mut *tx).await.map_err(DbError::from)?;
    let queued:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM model_assessment_runs WHERE state='queued' AND reserved_requests=0 AND owner_generation IS NULL ORDER BY created_at,id LIMIT 100")
        .fetch_all(&mut *tx).await.map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(queued)
}
