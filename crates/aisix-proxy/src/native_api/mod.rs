//! 原生协议生命周期：归属、原始账号边界与异步结算共用控制面契约。

pub mod background;
pub(crate) mod openai;
pub mod openai_batch_worker;
pub(crate) mod openai_chat;
pub(crate) mod openai_uploads;
pub(crate) mod origin;
pub mod registry;
pub(crate) mod responses;

pub(crate) mod anthropic;
pub mod anthropic_batch_worker;
pub(crate) mod anthropic_batches;
