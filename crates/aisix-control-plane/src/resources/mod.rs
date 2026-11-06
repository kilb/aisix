//! Gateway resources: the documents the control plane projects to kine for
//! the gateways to load.
//!
//! Postgres holds them in `gw_resources`, with credentials sealed. Every
//! write goes through [`write::write_resource`], which normalises a document
//! ([`normalize`]), validates it by the gateway's own schemas and types
//! ([`validate`]), checks its references ([`refs`]) and seals its
//! credentials ([`secrets`]) before storing it ([`store`]). [`api`] is the
//! operators' generic API over it.

pub mod api;
pub mod kinds;
pub mod normalize;
pub mod refs;
pub mod secrets;
pub mod store;
pub mod validate;
pub mod write;

pub use kinds::Kind;
pub use refs::Referrer;

use crate::error::ApiError;

/// `409 managed_resource`: the document is derived from tenants, members or
/// keys, and only the code deriving it changes it.
pub(crate) fn managed_resource() -> ApiError {
    ApiError::conflict(
        "managed_resource",
        "该资源由平台根据租户、成员或密钥自动维护，不能在此直接修改",
    )
}
