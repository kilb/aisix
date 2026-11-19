use axum::extract::{Path, State};
use axum::Json;
use chrono::Utc;
use serde::Serialize;
use uuid::Uuid;

use super::{
    effective_disabled, effective_groups, key_id, live_key, member_active, own_only, reach,
    KeyReach,
};
use crate::auth::{MemberAuth, KEY_HOLDERS};
use crate::db::DbError;
use crate::error::ApiError;
use crate::resources::Kind;
use crate::{tenants, AppState};

// 客户密钥的新旧端点权限均开放这三个入口，网关在文本协议之间提供转换。
const TEXT_PROTOCOLS: [&str; 3] = ["chat_completions", "messages", "responses"];

#[derive(Serialize)]
pub struct QuickstartModel {
    id: Uuid,
    name: String,
    protocols: [&'static str; 3],
}

#[derive(Serialize)]
pub struct Quickstart {
    items: Vec<QuickstartModel>,
}

/// `GET /api/keys/{id}/quickstart`：只返回当前密钥可以选择的文本示例。
pub async fn quickstart(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Quickstart>, ApiError> {
    auth.require_active()?;
    auth.require(KEY_HOLDERS)?;
    let id = key_id(&id)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let key = live_key(&mut conn, id, Some(auth.tenant_id), own_only(&auth)).await?;
    let tenant = tenants::find(&mut conn, auth.tenant_id).await?;
    let owner_active = member_active(&mut conn, key.member_id).await?;
    if effective_disabled(&tenant, &key, owner_active)
        || key.expires_at.is_some_and(|expiry| expiry <= Utc::now())
    {
        return Ok(Json(Quickstart { items: Vec::new() }));
    }

    let reach = reach(&tenant, &key);
    let grants = match &reach {
        KeyReach::Ids(ids) => Some(ids.as_slice()),
        KeyReach::Names(_) => None,
    };
    // 公开名称必须在密钥授权内，且至少有一个启用的文本目标可在密钥分组内使用。
    // 路由目标可以通过 ID 或名称引用；有 ID 时以 ID 为准。
    let models: Vec<(Uuid, String)> = sqlx::query_as(
        "WITH live_targets AS MATERIALIZED ( \
           SELECT target.id, target.id::text AS ref_id, target.doc->>'display_name' AS name \
           FROM gw_resources target JOIN gw_resources account \
             ON account.kind = $2 AND account.id::text = target.doc->>'provider_key_id' \
           WHERE target.kind = $1 AND NOT target.deleted AND NOT account.deleted \
             AND (target.doc->>'embedding') IS NULL \
             AND account.doc->'disabled' IS DISTINCT FROM 'true'::jsonb \
             AND (cardinality($4::text[]) = 0 OR account.doc->'groups' ?| $4) \
         ), visible_models AS MATERIALIZED ( \
           SELECT model.id, model.doc->>'display_name' AS name, model.doc #> '{routing,targets}' AS targets \
           FROM gw_resources model LEFT JOIN public_models public ON public.id = model.id \
           WHERE model.kind = $1 AND NOT model.deleted \
             AND model.doc->'internal' IS DISTINCT FROM 'true'::jsonb \
             AND COALESCE(model.doc->>'display_name', '') <> '' \
             AND position('*' in model.doc->>'display_name') = 0 \
             AND ($3::uuid[] IS NULL OR model.id = ANY($3)) \
             AND (public.id IS NULL OR (public.enabled AND public.retired_at IS NULL)) \
         ), route_refs AS MATERIALIZED ( \
           SELECT model.id, route->>'model_id' AS target_id, route->>'model' AS target_name \
           FROM visible_models model CROSS JOIN LATERAL jsonb_array_elements( \
             CASE WHEN jsonb_typeof(model.targets) = 'array' THEN model.targets END) route \
         ), reachable AS MATERIALIZED ( \
           SELECT id FROM live_targets \
           UNION \
           SELECT route.id FROM route_refs route JOIN live_targets target ON target.ref_id = route.target_id \
           UNION \
           SELECT route.id FROM route_refs route JOIN live_targets target ON target.name = route.target_name \
           WHERE route.target_id IS NULL \
         ) \
         SELECT model.id, model.name FROM visible_models model \
         WHERE model.id IN (SELECT id FROM reachable) ORDER BY model.name, model.id",
    )
    .bind(Kind::Models.segment())
    .bind(Kind::ProviderKeys.segment())
    .bind(grants)
    .bind(effective_groups(&tenant, &key))
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let items = models
        .into_iter()
        .filter(|(_, name)| grants.is_some() || reach.names(name))
        .map(|(id, name)| QuickstartModel {
            id,
            name,
            protocols: TEXT_PROTOCOLS,
        })
        .collect();
    Ok(Json(Quickstart { items }))
}
