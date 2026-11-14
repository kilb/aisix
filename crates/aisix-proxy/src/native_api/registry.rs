//! 托管原生对象的控制面接口；归属判断不依赖进程内缓存。

use aisix_core::native_objects::{
    BeginRequest, BindRequest, ClaimRequest, CompleteRequest, LeaseRequest, LookupRequest,
    NativeReply, OwnedRequest, TombstoneRequest,
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

pub trait NativeObjectRegistry: Send + Sync {
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
}

pub(crate) fn proxy_error(error: NativeRegistryError) -> crate::error::ProxyError {
    match error {
        NativeRegistryError::NotFound => crate::error::ProxyError::ResponseNotFound,
        NativeRegistryError::Unavailable
        | NativeRegistryError::Conflict
        | NativeRegistryError::InvalidRequest => crate::error::ProxyError::AccountUnavailable,
    }
}
