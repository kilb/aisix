//! 仅通过网关 mTLS 身份访问原生对象的持久归属服务。

use aisix_core::native_objects::*;
use axum::extract::State;
use axum::Json;
use futures::FutureExt;

use super::DpIdentity;
use crate::error::ApiError;
use crate::http::ApiJson;
use crate::native_objects::store::{self, Command};
use crate::AppState;

// 原生批次完整终态包含至多 1000 项错误和用量，仍以固定上限接收。
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

async fn run(
    state: AppState,
    dp: DpIdentity,
    command: Command,
) -> Result<Json<NativeReply>, ApiError> {
    let db = state.native_objects.clone();
    db.tx(move |conn| {
        async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                store::execute(conn, &state, dp, command),
            )
            .await
            .map_err(|_| ApiError::unavailable())?
        }
        .boxed()
    })
    .await
    .map_err(|error: ApiError| {
        if error.status == axum::http::StatusCode::INTERNAL_SERVER_ERROR {
            ApiError::unavailable()
        } else {
            error
        }
    })
    .map(Json)
}

pub async fn begin(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<BeginRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Begin(Box::new(body))).await
}

pub async fn bind(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<BindRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Bind(body)).await
}

pub async fn lookup(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<LookupRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Lookup(body)).await
}

pub async fn owned(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<OwnedRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Owned(body)).await
}

pub async fn tombstone(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<TombstoneRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Tombstone(body)).await
}

pub async fn claim(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ClaimRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Claim(body)).await
}

pub async fn renew(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<LeaseRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Renew(body)).await
}

pub async fn reschedule(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<LeaseRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Reschedule(body)).await
}

pub async fn complete(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CompleteRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Complete(body)).await
}
