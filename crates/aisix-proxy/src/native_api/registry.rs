//! 托管原生对象的控制面接口；归属判断不依赖进程内缓存。

use aisix_core::native_metering::{ReconciliationRequest, TypedCompleteRequest};
use aisix_core::native_objects::{
    BeginRelatedRequest, BeginRequest, BindObservedArtifactsRequest, BindRelatedRequest,
    BindRequest, ClaimRequest, CommitRelationsRequest, CompleteRequest, LeaseRequest,
    LookupRelationsRequest, LookupRequest, NativeReply, ObjectDeletionRequest, OwnedRequest,
    ReserveRelationsRequest, ResolveReservationRequest, TombstoneRequest,
};
use aisix_core::native_realtime::SignedSipIntakeRequest;
use aisix_core::native_realtime::{
    AppendLiveUsageRequest, CheckpointLiveSessionRequest, ClaimLiveSessionsRequest,
    CreateLiveSessionRequest, MintCredentialRequest, RedeemCredentialRequest,
};
use futures::future::BoxFuture;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NativeRegistryError {
    #[error("native object authority is unavailable")]
    Unavailable,
    #[error("native object was not found")]
    NotFound,
    #[error("native object operation conflicts")]
    Conflict,
    #[error("native object request is invalid")]
    InvalidRequest,
}

/// 成功持久追加后的唯一计费出口；不能从部署模式或本地密钥猜测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveUsageAuthority {
    Unsupported,
    AggregateSink,
    CanonicalJournal,
}

pub trait NativeObjectRegistry: Send + Sync {
    /// 未声明的后端不承担实时计费；新会话不能据此同时使用两条付费出口。
    fn live_usage_authority(&self) -> LiveUsageAuthority {
        LiveUsageAuthority::Unsupported
    }

    /// 本地归属密钥独立于平台错误隐藏模式；托管实现继续使用平台配置。
    fn id_key(&self) -> Option<[u8; 32]> {
        None
    }
    /// 配置资源 ID 转为内部身份；客户端上游对象 ID 从不进入该映射。
    fn resource_id(&self, _kind: &str, raw: &str) -> Result<uuid::Uuid, NativeRegistryError> {
        uuid::Uuid::parse_str(raw).map_err(|_| NativeRegistryError::InvalidRequest)
    }

    fn begin(
        &self,
        request: BeginRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>>;
    fn bind(&self, request: BindRequest)
        -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>>;
    fn lookup(
        &self,
        request: LookupRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>>;
    fn owned(
        &self,
        request: OwnedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>>;
    fn tombstone(
        &self,
        request: TombstoneRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>>;
    fn claim(
        &self,
        request: ClaimRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>>;
    fn renew(
        &self,
        request: LeaseRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>>;
    fn reschedule(
        &self,
        request: LeaseRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>>;
    fn complete(
        &self,
        request: CompleteRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>>;
    fn signed_sip_intake(
        &self,
        _request: SignedSipIntakeRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn mint_credential(
        &self,
        _request: MintCredentialRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn redeem_credential(
        &self,
        _request: RedeemCredentialRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn create_live_session(
        &self,
        _request: CreateLiveSessionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn append_live_usage(
        &self,
        _request: AppendLiveUsageRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn checkpoint_live_session(
        &self,
        _request: CheckpointLiveSessionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn claim_live_sessions(
        &self,
        _request: ClaimLiveSessionsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn reserve_delete(
        &self,
        _request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn commit_delete(
        &self,
        _request: ObjectDeletionRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn complete_typed(
        &self,
        _request: TypedCompleteRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn mark_reconciliation(
        &self,
        _request: ReconciliationRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn begin_related(
        &self,
        _request: BeginRelatedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn bind_related(
        &self,
        _request: BindRelatedRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    /// 旧 backend 缺少持久关系事务时必须拒绝，不能退化为内存授权。
    fn reserve_relations(
        &self,
        _request: ReserveRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn commit_relations(
        &self,
        _request: CommitRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn lookup_relations(
        &self,
        _request: LookupRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn resolve_reservation(
        &self,
        _request: ResolveReservationRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
    fn bind_observed_artifacts(
        &self,
        _request: BindObservedArtifactsRequest,
    ) -> BoxFuture<'_, Result<NativeReply, NativeRegistryError>> {
        Box::pin(async { Err(NativeRegistryError::Unavailable) })
    }
}

pub(crate) fn proxy_error(error: NativeRegistryError) -> crate::error::ProxyError {
    match error {
        NativeRegistryError::NotFound => crate::error::ProxyError::ResponseNotFound,
        NativeRegistryError::Unavailable
        | NativeRegistryError::Conflict
        | NativeRegistryError::InvalidRequest => crate::error::ProxyError::AccountUnavailable,
    }
}
