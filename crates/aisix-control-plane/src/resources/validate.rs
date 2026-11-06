//! Validation by the gateway's own rules: the strict schema `schemas/resources`
//! is generated from, the typed model the gateway deserializes, and the
//! semantic checks its loader runs after that
//! (`crates/aisix-etcd/src/loader.rs`).

use aisix_core::models::schema::{self, SchemaError};
use aisix_core::models::{
    ApiKey, Budget, BudgetPolicy, CachePolicy, Guardrail, GuardrailAttachment, Model,
    ObservabilityExporter, ProviderKey, RateLimitPolicy, Team, User,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::Kind;
use crate::error::Detail;

/// Checks `doc` the way the gateway's write path does: the strict schema,
/// typed deserialization, and the kind's semantic checks.
pub fn validate_strict(kind: Kind, doc: &Value) -> Result<(), Vec<Detail>> {
    let checked = match kind {
        Kind::Models => schema::validate_model(doc),
        Kind::ProviderKeys => schema::validate_provider_key(doc),
        Kind::CachePolicies => schema::validate_cache_policy(doc),
        Kind::RateLimitPolicies => schema::validate_rate_limit_policy(doc),
        Kind::Guardrails => schema::validate_guardrail(doc),
        Kind::GuardrailAttachments => schema::validate_guardrail_attachment(doc),
        Kind::ObservabilityExporters => schema::validate_observability_exporter(doc),
        Kind::BudgetPolicy => schema::validate_budget_policy(doc),
        Kind::Teams => schema::validate_team(doc),
        Kind::Users => schema::validate_user(doc),
        Kind::ApiKeys => schema::validate_apikey(doc),
        Kind::Budgets => schema::validate_budget(doc),
    };
    // The validator masks instance values in its messages, so the reason
    // can be shown as it is.
    checked.map_err(|e| vec![invalid(e.path, &e.message)])?;
    match kind {
        Kind::Models => parse::<Model>(doc).map(drop),
        Kind::ProviderKeys => parse::<ProviderKey>(doc).map(drop),
        Kind::CachePolicies => parse::<CachePolicy>(doc).map(drop),
        Kind::RateLimitPolicies => parse::<RateLimitPolicy>(doc)?
            .validate_semantics()
            .map_err(|reason| vec![invalid(String::new(), &reason)]),
        Kind::Guardrails => parse::<Guardrail>(doc).map(drop),
        Kind::GuardrailAttachments => parse::<GuardrailAttachment>(doc).map(drop),
        Kind::ObservabilityExporters => parse::<ObservabilityExporter>(doc).map(drop),
        Kind::BudgetPolicy => parse::<BudgetPolicy>(doc).map(drop),
        Kind::Teams => parse::<Team>(doc).map(drop),
        Kind::Users => parse::<User>(doc).map(drop),
        Kind::ApiKeys => parse::<ApiKey>(doc).map(drop),
        Kind::Budgets => parse::<Budget>(doc).map(drop),
    }
}

/// Checks `doc` against the schema the gateway loads stored documents with
/// (`schemas/resources-lenient`): what a projected document must pass for a
/// gateway of this release to serve it.
pub fn validate_lenient(kind: Kind, doc: &Value) -> Result<(), SchemaError> {
    match kind {
        Kind::Models => schema::validate_model_lenient(doc),
        Kind::ProviderKeys => schema::validate_provider_key_lenient(doc),
        Kind::CachePolicies => schema::validate_cache_policy_lenient(doc),
        Kind::RateLimitPolicies => schema::validate_rate_limit_policy_lenient(doc),
        Kind::Guardrails => schema::validate_guardrail_lenient(doc),
        Kind::GuardrailAttachments => schema::validate_guardrail_attachment_lenient(doc),
        Kind::ObservabilityExporters => schema::validate_observability_exporter_lenient(doc),
        Kind::BudgetPolicy => schema::validate_budget_policy_lenient(doc),
        Kind::Teams => schema::validate_team_lenient(doc),
        Kind::Users => schema::validate_user_lenient(doc),
        Kind::ApiKeys => schema::validate_apikey_lenient(doc),
        Kind::Budgets => schema::validate_budget_lenient(doc),
    }
}

fn invalid(path: String, reason: &str) -> Detail {
    Detail {
        path,
        message: format!("不符合网关的资源定义：{reason}"),
    }
}

/// The gateway's typed reading of `doc`. serde's own message is not passed
/// on: it can quote a value, and a document carries credentials.
fn parse<T: DeserializeOwned>(doc: &Value) -> Result<T, Vec<Detail>> {
    T::deserialize(doc).map_err(|_| vec![invalid(String::new(), "网关无法按其类型读取此文档")])
}
