//! The gateway resource kinds the control plane stores, and what each one
//! is: its etcd segment, its schema, whether it holds credentials, whether
//! operators write it directly, and the field its live rows are unique by.

use aisix_core::credentials::CredentialKind;
use serde::{Serialize, Serializer};

/// A gateway resource kind, named on the wire and in etcd by its
/// [`segment`](Kind::segment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Models,
    ProviderKeys,
    CachePolicies,
    RateLimitPolicies,
    Guardrails,
    GuardrailAttachments,
    ObservabilityExporters,
    BudgetPolicy,
    Teams,
    Users,
    ApiKeys,
    Budgets,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::Models,
        Kind::ProviderKeys,
        Kind::CachePolicies,
        Kind::RateLimitPolicies,
        Kind::Guardrails,
        Kind::GuardrailAttachments,
        Kind::ObservabilityExporters,
        Kind::BudgetPolicy,
        Kind::Teams,
        Kind::Users,
        Kind::ApiKeys,
        Kind::Budgets,
    ];

    /// The collection name in `<prefix>/<env_id>/<segment>/<id>` and in
    /// `/admin/resources/{kind}`.
    pub fn segment(self) -> &'static str {
        match self {
            Kind::Models => "models",
            Kind::ProviderKeys => "provider_keys",
            Kind::CachePolicies => "cache_policies",
            Kind::RateLimitPolicies => "rate_limit_policies",
            Kind::Guardrails => "guardrails",
            Kind::GuardrailAttachments => "guardrail_attachments",
            Kind::ObservabilityExporters => "observability_exporters",
            Kind::BudgetPolicy => "budget_policy",
            Kind::Teams => "teams",
            Kind::Users => "users",
            Kind::ApiKeys => "api_keys",
            Kind::Budgets => "budgets",
        }
    }

    pub fn from_segment(segment: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.segment() == segment)
    }

    /// The name `aisix_core::models::schema` knows the kind's schema by (an
    /// entry of `schema::RESOURCES`).
    pub fn schema_name(self) -> &'static str {
        match self {
            Kind::Models => "model",
            Kind::ProviderKeys => "provider_key",
            Kind::CachePolicies => "cache_policy",
            Kind::RateLimitPolicies => "rate_limit_policy",
            Kind::Guardrails => "guardrail",
            Kind::GuardrailAttachments => "guardrail_attachment",
            Kind::ObservabilityExporters => "observability_exporter",
            Kind::BudgetPolicy => "budget_policy",
            Kind::Teams => "team",
            Kind::Users => "user",
            Kind::ApiKeys => "api_key",
            Kind::Budgets => "budget",
        }
    }

    /// Where the kind's documents keep credentials, if they keep any.
    ///
    /// Every kind is named, so a kind added later is a compile error here
    /// until someone decides whether its credentials are sealed and
    /// redacted.
    pub fn credential_kind(self) -> Option<CredentialKind> {
        match self {
            Kind::ProviderKeys => Some(CredentialKind::ProviderKey),
            Kind::Guardrails => Some(CredentialKind::Guardrail),
            Kind::ObservabilityExporters => Some(CredentialKind::ObservabilityExporter),
            Kind::Models
            | Kind::CachePolicies
            | Kind::RateLimitPolicies
            | Kind::GuardrailAttachments
            | Kind::BudgetPolicy
            | Kind::Teams
            | Kind::Users
            | Kind::ApiKeys
            | Kind::Budgets => None,
        }
    }

    /// Whether operators write the kind through `/admin/resources`. The
    /// others are derived from tenants, members and keys, and only the code
    /// that derives them writes them.
    pub fn generic_writable(self) -> bool {
        !matches!(
            self,
            Kind::Teams | Kind::Users | Kind::ApiKeys | Kind::Budgets
        )
    }

    /// The field whose value is unique among the kind's live rows, if any.
    pub fn identity_field(self) -> Option<&'static str> {
        match self {
            Kind::Models | Kind::ProviderKeys => Some("display_name"),
            Kind::CachePolicies
            | Kind::RateLimitPolicies
            | Kind::Guardrails
            | Kind::ObservabilityExporters => Some("name"),
            Kind::ApiKeys => Some("key_hash"),
            _ => None,
        }
    }

    /// Whether the environment has at most one row of the kind, whose id is
    /// the `env_id`.
    pub fn is_singleton(self) -> bool {
        self == Kind::BudgetPolicy
    }
}

/// As its segment, which is how the API names a kind.
impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.segment())
    }
}

#[cfg(test)]
mod tests {
    use super::Kind;

    #[test]
    fn every_kind_has_one_segment_and_a_published_schema() {
        for kind in Kind::ALL {
            assert_eq!(Kind::from_segment(kind.segment()), Some(kind));
            assert!(
                aisix_core::models::schema::RESOURCES.contains(&kind.schema_name()),
                "{kind:?}"
            );
            assert_eq!(
                serde_json::to_value(kind).expect("serialize"),
                serde_json::json!(kind.segment())
            );
        }
        assert_eq!(Kind::from_segment("model"), None);
    }
}
