//! 管理员显式开始的异步能力对照；浏览、计划和存档读取都不触发推理。

pub mod admission;
pub mod collector;
pub mod contracts;
pub mod history;
pub mod runner;
pub mod scoring;
pub mod suite;
pub mod target;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::error::ApiError;
use crate::http::ApiJson;
use crate::AppState;
use contracts::*;

pub async fn suites(auth: OperatorAuth) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    Ok(Json(
        json!({"items":[{"id":"capability-v1","version":"1","dimensions":["reasoning","code","context","tool","protocol"],"quick":{"clusters_per_dimension":2,"requests":20},"full":{"clusters_per_dimension":16,"requests":160},"warnings":["suite_thresholds_not_calibrated","no_model_identity_certification"]}]}),
    ))
}
pub async fn plan(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<PlanRequest>,
) -> Result<(StatusCode, Json<AssessmentRun>), ApiError> {
    auth.require(OperatorRole::Admin)?;
    let run = history::create(&state, &auth, body).await?;
    Ok((StatusCode::CREATED, Json(run)))
}
pub async fn start(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ApiJson(body): ApiJson<StartRequest>,
) -> Result<(StatusCode, Json<AssessmentRun>), ApiError> {
    auth.require(OperatorRole::Admin)?;
    let run = history::start(&state, &auth, id, body).await?;
    runner::spawn(state, id);
    Ok((StatusCode::ACCEPTED, Json(run)))
}
pub async fn get(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AssessmentRun>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    Ok(Json(history::read(&state, id).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryQuery {
    pub limit: Option<u32>,
    pub before: Option<String>,
    pub model_id: Option<Uuid>,
}
fn limit(value: Option<u32>) -> Result<u32, ApiError> {
    let value = value.unwrap_or(50);
    if !(1..=100).contains(&value) {
        return Err(history::invalid("/limit", "limit必须为1到100"));
    }
    Ok(value)
}
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let Query(query) = query.map_err(|_| history::invalid("/query", "查询参数无效"))?;
    Ok(Json(
        history::list(
            &state,
            query.model_id,
            limit(query.limit)?,
            query.before.as_deref(),
        )
        .await?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamplesQuery {
    pub limit: Option<u32>,
    pub after: Option<String>,
}
pub async fn samples(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    query: Result<Query<SamplesQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    let Query(query) = query.map_err(|_| history::invalid("/query", "查询参数无效"))?;
    Ok(Json(
        history::samples(&state, id, limit(query.limit)?, query.after.as_deref()).await?,
    ))
}
pub async fn cancel(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ApiJson(_body): ApiJson<CancelRequest>,
) -> Result<Json<AssessmentRun>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    Ok(Json(history::cancel(&state, &auth, id).await?))
}
pub async fn export(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AssessmentExport>, ApiError> {
    auth.require(OperatorRole::Admin)?;
    Ok(Json(history::export(&state, id).await?))
}

/// 只回收已失联作业和领取从未预留的queued；绝不自动恢复收费工作。
pub fn spawn(state: AppState, mut stopping: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _=crate::stopped(&mut stopping)=>break,
                _=ticks.tick()=>{
                    match history::maintenance(&state).await {
                        Ok(ids)=>for id in ids {runner::spawn(state.clone(),id);},
                        Err(_)=>tracing::warn!("能力对照作业维护失败，本轮不派发"),
                    }
                }
            }
        }
    })
}
