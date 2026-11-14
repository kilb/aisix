//! SIP 入口只接受运维绑定的当前调用身份和精确上游账号。

use aisix_core::models::{AisixSnapshot, ApiKey, EndpointFamily, Model, ProviderKey};
use aisix_core::resource::ResourceEntry;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::write::{ResourceWrite, WriteCtx};
use super::{secrets, Kind};
use crate::db::DbError;
use crate::error::{ApiError, Detail};

fn invalid(field: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: format!("/realtime_sip/{field}"),
        message: message.to_string(),
    }])
}

/// 在资源锁内检查，不在此取得租户锁；调用密钥撤销仍可先于入口解除。
pub(super) async fn check(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    write: &ResourceWrite<'_>,
    projected: &Value,
    stored: &Value,
) -> Result<(), ApiError> {
    if write.kind != Kind::ProviderKeys || projected.get("realtime_sip").is_none_or(Value::is_null)
    {
        return Ok(());
    }
    let provider: ProviderKey =
        serde_json::from_value(projected.clone()).map_err(ApiError::internal)?;
    let Some(binding) = &provider.realtime_sip else {
        return Ok(());
    };
    let signing_key = binding
        .signing_key()
        .map_err(|message| invalid("webhook_secret", message))?;
    if stored
        .get("realtime_sip")
        .is_some_and(|value| !value.is_null())
        && write.expected_version.is_none()
    {
        return Err(ApiError::if_match_required());
    }
    let caller_id = Uuid::parse_str(&binding.api_key_id)
        .map_err(|_| invalid("api_key_id", "请选择平台调用密钥"))?;
    let model_id = Uuid::parse_str(&binding.model_id)
        .map_err(|_| invalid("model_id", "请选择平台直连模型"))?;
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tenant_keys k JOIN tenants t ON t.id=k.tenant_id \
         LEFT JOIN members m ON m.id=k.member_id AND m.tenant_id=k.tenant_id \
         WHERE k.id=$1 AND k.revoked_at IS NULL AND NOT k.disabled AND NOT k.operator_disabled \
         AND (k.expires_at IS NULL OR k.expires_at > clock_timestamp()) AND t.status='active' \
         AND (k.member_id IS NULL OR (m.removed_at IS NULL AND NOT m.disabled \
         AND m.role IN ('owner','admin','member'))))",
    )
    .bind(caller_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if !active {
        return Err(invalid("api_key_id", "调用密钥、所属租户或成员当前不可用"));
    }
    let caller_doc: Option<Value> = sqlx::query_scalar(
        "SELECT doc FROM gw_resources WHERE kind='api_keys' AND id=$1 AND NOT deleted",
    )
    .bind(caller_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let caller: ApiKey =
        serde_json::from_value(caller_doc.ok_or_else(|| invalid("api_key_id", "调用密钥不存在"))?)
            .map_err(ApiError::internal)?;
    let model_doc: Option<Value> = sqlx::query_scalar(
        "SELECT doc FROM gw_resources WHERE kind='models' AND id=$1 AND NOT deleted",
    )
    .bind(model_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let model: Model =
        serde_json::from_value(model_doc.ok_or_else(|| invalid("model_id", "直连模型不存在"))?)
            .map_err(ApiError::internal)?;
    if model.provider_key_id.as_deref() != Some(write.id.to_string().as_str())
        || model.is_routing()
        || model.is_ensemble()
        || model.is_semantic()
        || model
            .model_name
            .as_ref()
            .is_none_or(|name| name.trim().is_empty())
        || model.display_name.contains('*')
    {
        return Err(invalid("model_id", "模型必须是当前上游账号的精确直连模型"));
    }
    let snapshot = AisixSnapshot::new();
    let model_name = model.display_name.clone();
    snapshot
        .models
        .insert(ResourceEntry::new(&binding.model_id, model, 1));
    if caller.disabled
        || caller.is_expired_at(chrono::Utc::now())
        || !caller.can_call(EndpointFamily::Realtime)
        || !caller.can_access(&snapshot, &model_name)
        || (!caller.allowed_provider_groups.is_empty()
            && !caller
                .allowed_provider_groups
                .iter()
                .any(|group| provider.groups.contains(group)))
    {
        return Err(invalid(
            "api_key_id",
            "调用密钥当前无权访问该直连模型或 Realtime 接口",
        ));
    }
    // 静态项目头决定命名空间；不同 API key 不能把同一项目的 SIP 来电分租户。
    let namespace = provider
        .realtime_sip_namespace()
        .map_err(|message| invalid("model_id", message))?;
    let others: Vec<(Uuid, Value)> = sqlx::query_as(
        "SELECT id,doc FROM gw_resources WHERE kind='provider_keys' AND NOT deleted \
         AND id<>$1 AND doc->'realtime_sip' IS NOT NULL AND doc->'realtime_sip'<>'null'::jsonb",
    )
    .bind(write.id)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    for (id, mut doc) in others {
        if doc
            .pointer("/realtime_sip/route_id")
            .and_then(Value::as_str)
            == Some(&binding.route_id)
        {
            return Err(invalid("route_id", "入口标识已绑定其他上游账号"));
        }
        secrets::unseal(&ctx.mk, Kind::ProviderKeys, id, &mut doc).map_err(ApiError::internal)?;
        let other: ProviderKey = serde_json::from_value(doc).map_err(ApiError::internal)?;
        let Some(other_binding) = &other.realtime_sip else {
            continue;
        };
        let other_signing_key = other_binding
            .signing_key()
            .map_err(|message| invalid("webhook_secret", message))?;
        if other_signing_key == signing_key {
            return Err(invalid(
                "webhook_secret",
                "签名密钥已用于其他入口，请为专用项目配置独立签名密钥",
            ));
        }
        let other_namespace = other
            .realtime_sip_namespace()
            .map_err(|message| invalid("model_id", message))?;
        if other_namespace == namespace {
            return Err(invalid(
                "api_key_id",
                "同一上游项目账号只能绑定一个专用 SIP 入口",
            ));
        }
    }
    Ok(())
}
