//! 原生协议生命周期：归属、原始账号边界与异步结算共用控制面契约。

pub mod background;
pub(crate) mod openai;
pub mod openai_batch_worker;
pub(crate) mod openai_chat;
pub(crate) mod openai_containers;
pub(crate) mod openai_conversations;
pub mod openai_eval_worker;
pub(crate) mod openai_evals;
pub(crate) mod openai_uploads;
pub(crate) mod openai_vector_stores;
pub(crate) mod origin;
pub mod registry;
pub(crate) mod response_refs;
pub(crate) mod responses;
pub(crate) mod skills;
mod standalone_realtime_store;
pub mod standalone_store;
pub(crate) mod voices;

pub(crate) mod anthropic;
pub mod anthropic_batch_worker;
pub(crate) mod anthropic_batches;
