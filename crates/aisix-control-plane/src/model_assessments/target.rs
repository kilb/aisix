//! 冻结确切资源版本；计划读取不解封秘密，也不执行网络预检。

use aisix_core::{Adapter, Model, ProviderKey};
use serde_json::Value;
use uuid::Uuid;

use super::contracts::{SafeTargetSnapshot, TargetInput};
use crate::error::{ApiError, Detail};
use crate::model_probes::providers;
use crate::resources::{self, store, Kind};
use crate::AppState;

struct StoredTarget {
    model: Model,
    model_version: i64,
    account_id: Uuid,
    account_version: i64,
    account_doc: Value,
}

pub struct DispatchTarget {
    pub model: Model,
    pub account_id: Uuid,
    pub account: ProviderKey,
}

pub async fn resolve(
    state: &AppState,
    input: &TargetInput,
) -> Result<SafeTargetSnapshot, ApiError> {
    let stored = load(state, input.model_id).await?;
    check_versions(&stored, &input.model_version, &input.provider_key_version)?;
    let account = parse_account(&stored.account_doc)?;
    validate(&stored.model, &account)?;
    let adapter = resolved_adapter(&account)?;
    let endpoint = match adapter {
        Adapter::Anthropic => account
            .apis
            .as_ref()
            .and_then(|apis| apis.messages.as_ref())
            .and_then(|endpoint| endpoint.base.as_deref())
            .or(account.api_base.as_deref())
            .unwrap_or("https://api.anthropic.com"),
        Adapter::Openai => account
            .api_base
            .as_deref()
            .unwrap_or("https://api.openai.com/v1"),
        _ => account.api_base.as_deref().unwrap_or_default(),
    };
    Ok(SafeTargetSnapshot {
        model_id: input.model_id,
        model_version: stored.model_version.to_string(),
        provider_key_version: stored.account_version.to_string(),
        model_name: safe_label(&stored.model.display_name),
        account_id: stored.account_id,
        account_name: safe_label(&account.display_name),
        provider: account.provider.clone(),
        adapter: adapter.wire_protocol().to_string(),
        configured_model: stored.model.model_name.clone().unwrap_or_default(),
        endpoint_display: if endpoint.is_empty() {
            String::new()
        } else {
            aisix_core::redact_url_for_log(endpoint)
        },
    })
}

pub async fn verify_revisions(
    state: &AppState,
    target: &SafeTargetSnapshot,
) -> Result<(), ApiError> {
    let stored = load(state, target.model_id).await?;
    check_snapshot(&stored, target)
}

/// 只有预留成功后才解封；不缓存账号，不能换用新版本继续旧 run。
pub async fn for_dispatch(
    state: &AppState,
    target: &SafeTargetSnapshot,
) -> Result<DispatchTarget, ApiError> {
    let mut stored = load(state, target.model_id).await?;
    check_snapshot(&stored, target)?;
    resources::secrets::unseal(
        &state.cfg.master_key,
        Kind::ProviderKeys,
        stored.account_id,
        &mut stored.account_doc,
    )
    .map_err(|_| ApiError::internal("能力对照账号解封失败"))?;
    let account = parse_account(&stored.account_doc)?;
    validate(&stored.model, &account)?;
    Ok(DispatchTarget {
        model: stored.model,
        account_id: stored.account_id,
        account,
    })
}

pub async fn comparability(
    state: &AppState,
    target: &SafeTargetSnapshot,
) -> Result<Vec<String>, ApiError> {
    let stored = load(state, target.model_id).await?;
    check_snapshot(&stored, target)?;
    let account = parse_account(&stored.account_doc)?;
    validate(&stored.model, &account)?;
    Ok(comparability_reasons(&account))
}

async fn load(state: &AppState, model_id: Uuid) -> Result<StoredTarget, ApiError> {
    let mut conn = state
        .db
        .pool()
        .acquire()
        .await
        .map_err(crate::db::DbError::from)?;
    let row = store::get(&mut conn, Kind::Models, model_id)
        .await?
        .filter(|row| !row.deleted)
        .ok_or_else(ApiError::not_found)?;
    let model: Model = serde_json::from_value(row.doc)
        .map_err(|_| ApiError::internal("能力对照模型配置无法读取"))?;
    let account_id = model
        .provider_key_id
        .as_deref()
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| invalid("/model_id", "请选择有确切账号的直连文本模型"))?;
    let account = store::get(&mut conn, Kind::ProviderKeys, account_id)
        .await?
        .filter(|row| !row.deleted)
        .ok_or_else(ApiError::not_found)?;
    Ok(StoredTarget {
        model,
        model_version: row.version,
        account_id,
        account_version: account.version,
        account_doc: account.doc,
    })
}

fn parse_account(doc: &Value) -> Result<ProviderKey, ApiError> {
    serde_json::from_value(doc.clone()).map_err(|_| ApiError::internal("能力对照账号配置无法读取"))
}

fn check_versions(stored: &StoredTarget, model: &str, account: &str) -> Result<(), ApiError> {
    // 字符串必须是资源实际返回的十进制版本，不能通过整数宽松解析接受另一种拼法。
    if stored.model_version.to_string() != model || stored.account_version.to_string() != account {
        return Err(changed());
    }
    Ok(())
}

fn check_snapshot(stored: &StoredTarget, target: &SafeTargetSnapshot) -> Result<(), ApiError> {
    check_versions(stored, &target.model_version, &target.provider_key_version)?;
    if stored.account_id != target.account_id {
        return Err(changed());
    }
    Ok(())
}

fn validate(model: &Model, account: &ProviderKey) -> Result<(), ApiError> {
    if model.is_routing()
        || model.is_ensemble()
        || model.is_semantic()
        || model.is_embedding()
        || model.display_name.contains('*')
        || model
            .model_name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty() || name.contains('*'))
    {
        return Err(invalid("/model_id", "能力对照只接受确切的直连文本模型"));
    }
    if account
        .request
        .as_ref()
        .is_some_and(|request| request.rewrites_body())
    {
        return Err(invalid(
            "/provider_key_id",
            "能力对照不接受非空的请求正文覆盖配置",
        ));
    }
    if model
        .effort_mapping
        .as_ref()
        .is_some_and(|mapping| !mapping.is_empty())
    {
        return Err(invalid(
            "/model_id",
            "能力对照不接受会改写推理参数的模型配置",
        ));
    }
    resolved_adapter(account)?;
    Ok(())
}

fn comparability_reasons(account: &ProviderKey) -> Vec<String> {
    let Some(request) = &account.request else {
        return Vec::new();
    };
    if request
        .default_headers
        .keys()
        .any(|name| !transport_or_credential_header(name))
    {
        vec!["configuration_incomparable:semantic_headers".to_string()]
    } else {
        Vec::new()
    }
}

fn transport_or_credential_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "api-key"
            | "x-goog-api-key"
            | "cookie"
            | "x-amz-security-token"
            | "x-amz-date"
            | "x-amz-content-sha256"
            | "accept"
            | "content-type"
            | "user-agent"
            | "anthropic-version"
            | "traceparent"
            | "tracestate"
            | "x-request-id"
            | "x-correlation-id"
            | "x-client-request-id"
            | "x-ms-client-request-id"
    )
}

fn resolved_adapter(account: &ProviderKey) -> Result<Adapter, ApiError> {
    providers::adapter(account)
        .map_err(|_| invalid("/provider_key_id", "账号没有可用于能力对照的协议适配器"))
}

fn safe_label(value: &str) -> String {
    aisix_core::redact_userinfo(
        &aisix_gateway::upstream_error_scrub::scrub_upstream_error_text(value),
    )
    .into_owned()
}

fn invalid(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.to_string(),
    }])
}

fn changed() -> ApiError {
    ApiError::conflict(
        "configuration_changed",
        "模型或账号版本已变化，请重新创建计划",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> Model {
        serde_json::from_value(json!({"display_name":"concrete", "model_name":"claimed-model"}))
            .unwrap()
    }

    fn account(request: Value) -> ProviderKey {
        serde_json::from_value(json!({"display_name":"account", "provider":"custom", "adapter":"openai", "api_key":"sealed-secret", "request":request})).unwrap()
    }

    #[test]
    fn target_checks_reject_body_mutations_including_defaults_and_identity_renames() {
        for request in [
            json!({"default_body_fields":{"seed":7}}),
            json!({"param_renames":{"model":"model"}}),
            json!({"param_constraints":{"temperature_max":1.0}}),
        ] {
            assert!(validate(&model(), &account(request)).is_err());
        }
        assert!(validate(&model(), &account(json!({}))).is_ok());
    }

    #[test]
    fn semantic_headers_invalidate_comparison_without_disclosing_names_or_values() {
        let account =
            account(json!({"default_headers":{"x-custom-secret-policy":"private-value"}}));
        let reasons = comparability_reasons(&account);
        assert_eq!(reasons, ["configuration_incomparable:semantic_headers"]);
        assert!(!reasons.join(",").contains("private-value"));
        assert!(comparability_reasons(&self::account(
            json!({"default_headers":{"authorization":"secret", "x-request-id":"trace"}})
        ))
        .is_empty());
    }

    #[test]
    fn embedding_and_wildcard_targets_never_reach_dispatch() {
        let account = account(json!({}));
        let mut wildcard = model();
        wildcard.model_name = Some("model-*".into());
        assert!(validate(&wildcard, &account).is_err());
        let embedding: Model = serde_json::from_value(
            json!({"display_name":"embedding", "model_name":"embed", "embedding":{"dimensions":1536}}),
        )
        .unwrap();
        assert!(validate(&embedding, &account).is_err());
    }
}
