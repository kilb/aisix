//! 计数配置只引用现有账号与精确源模型；在共用资源锁内核对保存时的快照。

use aisix_core::models::{
    token_count_provider_serves, Adapter, ApiSurface, Model, ProviderKey, TokenCounting,
};
use serde_json::Value;
use sqlx::PgConnection;

use super::write::ResourceWrite;
use super::Kind;
use crate::db::DbError;
use crate::error::{ApiError, Detail};

fn invalid(path: String, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path,
        message: message.to_string(),
    }])
}

pub(super) async fn check(
    conn: &mut PgConnection,
    write: &ResourceWrite<'_>,
    doc: &Value,
    stored: &Value,
) -> Result<Option<&'static str>, ApiError> {
    if write.kind == Kind::ProviderKeys && !stored.is_null() {
        let provider: ProviderKey =
            serde_json::from_value(doc.clone()).map_err(ApiError::internal)?;
        let before: ProviderKey =
            serde_json::from_value(stored.clone()).map_err(ApiError::internal)?;
        if uses_default_responses(&provider) && !uses_default_responses(&before) {
            let referenced: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM gw_resources WHERE kind='token_counting' \
                 AND NOT deleted AND doc #>> '{responses_input_tokens,provider_key_id}' = $1)",
            )
            .bind(write.id.to_string())
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::from)?;
            return Ok(referenced.then_some("/apis"));
        }
    }
    if write.kind != Kind::TokenCounting {
        return Ok(None);
    }
    let mut default_responses = None;
    let config: TokenCounting = serde_json::from_value(doc.clone()).map_err(ApiError::internal)?;
    for (block, surface, target) in [
        (
            "messages_count_tokens",
            ApiSurface::Messages,
            config.messages_count_tokens,
        ),
        (
            "responses_input_tokens",
            ApiSurface::Responses,
            config.responses_input_tokens,
        ),
    ] {
        let Some(target) = target else { continue };
        // 引用存在性已由 refs::check_outgoing 在同一资源锁内验证。
        let provider: Value = sqlx::query_scalar("SELECT doc FROM gw_resources WHERE kind='provider_keys' AND id=$1::uuid AND NOT deleted")
            .bind(&target.provider_key_id).fetch_one(&mut *conn).await.map_err(DbError::from)?;
        let provider: ProviderKey = serde_json::from_value(provider).map_err(ApiError::internal)?;
        if !token_count_provider_serves(&provider, surface) {
            return Err(invalid(
                format!("/{block}/provider_key_id"),
                "所选账号不直接提供此计数接口的协议",
            ));
        }
        // 旧计数构建认识此资源，却不认识自定义账号省略 apis 的 Responses 默认值。
        if surface == ApiSurface::Responses
            && uses_default_responses(&provider)
            && stored.get(block) != doc.get(block)
        {
            default_responses = Some("/responses_input_tokens");
        }
        for (index, mapping) in target.model_mappings.iter().enumerate() {
            let path = format!("/{block}/model_mappings/{index}");
            let model: Value = sqlx::query_scalar(
                "SELECT doc FROM gw_resources WHERE kind='models' AND id=$1::uuid AND NOT deleted",
            )
            .bind(&mapping.model_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::from)?;
            let model: Model = serde_json::from_value(model).map_err(ApiError::internal)?;
            if model.is_routing()
                || model.is_ensemble()
                || model.is_semantic()
                || model.is_embedding()
                || model.display_name.contains('*')
                || model
                    .model_name
                    .as_deref()
                    .is_none_or(|name| name.trim().is_empty() || name.contains('*'))
                || model.provider_key_id.is_none()
            {
                return Err(invalid(
                    format!("{path}/model_id"),
                    "请选择已有的精确文本直连模型，不能使用通配、路由、语义、集成或嵌入模型",
                ));
            }
            if model.model_name.as_deref() != Some(mapping.source_model_name.as_str()) {
                return Err(invalid(
                    format!("{path}/source_model_name"),
                    "源模型已变化，请重新选择模型并更新映射",
                ));
            }
        }
    }
    Ok(default_responses)
}

fn uses_default_responses(provider: &ProviderKey) -> bool {
    provider.provider != "openai"
        && provider.adapter == Some(Adapter::Openai)
        && provider.apis.is_none()
}

/// 晚到的旧实例也必须按存活配置所需的能力显示，不能只看资源是否已加载。
pub(crate) async fn default_responses_live(conn: &mut PgConnection) -> Result<bool, DbError> {
    let provider: Option<sqlx::types::Json<ProviderKey>> = sqlx::query_scalar(
        "SELECT p.doc FROM gw_resources t JOIN gw_resources p \
         ON p.kind='provider_keys' AND p.id::text = t.doc #>> '{responses_input_tokens,provider_key_id}' \
         AND NOT p.deleted WHERE t.kind='token_counting' AND NOT t.deleted",
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(provider.is_some_and(|provider| uses_default_responses(&provider.0)))
}
