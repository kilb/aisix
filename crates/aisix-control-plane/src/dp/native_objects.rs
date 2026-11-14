//! 仅通过网关 mTLS 身份访问原生对象的持久归属服务。

use aisix_core::native_metering::{ReconciliationRequest, TypedCompleteRequest};
use aisix_core::native_objects::*;
use aisix_core::native_realtime::SignedSipIntakeRequest;
use aisix_core::native_realtime::{
    AppendLiveUsageRequest, CheckpointLiveSessionRequest, ClaimLiveSessionsRequest,
    CreateLiveSessionRequest, MintCredentialRequest, RedeemCredentialRequest,
};
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

pub async fn begin_related(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<BeginRelatedRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::BeginRelated(body)).await
}

pub async fn bind_related(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<BindRelatedRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::BindRelated(body)).await
}

pub async fn reserve_relations(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ReserveRelationsRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::ReserveRelations(body)).await
}

pub async fn commit_relations(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CommitRelationsRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::CommitRelations(body)).await
}

pub async fn lookup_relations(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<LookupRelationsRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::LookupRelations(body)).await
}

pub async fn resolve_reservation(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ResolveReservationRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::ResolveReservation(body)).await
}

pub async fn bind_observed_artifacts(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<BindObservedArtifactsRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::BindObservedArtifacts(body)).await
}

pub async fn reserve_delete(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ObjectDeletionRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::ReserveDelete(body)).await
}

pub async fn commit_delete(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ObjectDeletionRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::CommitDelete(body)).await
}

pub async fn complete_typed(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<TypedCompleteRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::CompleteTyped(body)).await
}

pub async fn mark_reconciliation(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ReconciliationRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::Reconciliation(body)).await
}

pub async fn mint_credential(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<MintCredentialRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::MintCredential(body)).await
}

pub async fn redeem_credential(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<RedeemCredentialRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::RedeemCredential(body)).await
}

pub async fn create_live_session(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateLiveSessionRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::CreateLiveSession(body)).await
}

pub async fn append_live_usage(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<AppendLiveUsageRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::AppendLiveUsage(body)).await
}

pub async fn checkpoint_live_session(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CheckpointLiveSessionRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::CheckpointLiveSession(body)).await
}

pub async fn claim_live_sessions(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ClaimLiveSessionsRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::ClaimLiveSessions(body)).await
}

pub async fn signed_sip_intake(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SignedSipIntakeRequest>,
) -> Result<Json<NativeReply>, ApiError> {
    run(state, dp, Command::SignedSipIntake(body)).await
}
