//! 计数配置只引用现有账号与精确源模型；在共用资源锁内核对保存时的快照。

use aisix_core::models::{
    token_count_provider_serves, ApiSurface, Model, ProviderKey, TokenCounting,
};
use serde_json::Value;
use sqlx::PgConnection;

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
    kind: Kind,
    doc: &Value,
) -> Result<(), ApiError> {
    if kind != Kind::TokenCounting {
        return Ok(());
    }
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
    Ok(())
}
