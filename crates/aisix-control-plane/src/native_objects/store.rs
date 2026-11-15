//! 归属不可改写；提交写先串行同一操作，再按租户、提交、对象加锁，不拿资源全局锁。

use aisix_core::native_realtime::SignedSipIntakeRequest;
use std::collections::{BTreeSet, HashMap};

use aisix_core::models::EndpointFamily;
use aisix_core::native_objects::*;
use aisix_core::{ApiKey, Model, ProviderKey};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::billing::access::{self, Visibility};
use crate::db::DbError;
use crate::dp::DpIdentity;
use crate::error::{ApiError, Detail};
use crate::keys::{self, KeyReach, KeyRow};
use crate::resources::{secrets, Kind};
use crate::tenants::{self, TenantRow};
use crate::AppState;

use super::{LEASE_SECONDS, MAX_ENTRIES, MAX_PAGE};

mod deletion;
mod foundation;
mod metering;
mod realtime;
mod relations;
use aisix_core::native_metering::{MeteringTarget, ReconciliationRequest, TypedCompleteRequest};
use aisix_core::native_realtime::{
    AppendLiveUsageRequest, CheckpointLiveSessionRequest, ClaimLiveSessionsRequest,
    CreateLiveSessionRequest, MintCredentialRequest, RedeemCredentialRequest,
};
use foundation::{derive_object, insert_membership};

#[cfg(test)]
#[path = "store/begin_perf_tests.rs"]
mod begin_perf_tests;

pub(crate) enum Command {
    Begin(Box<BeginRequest>),
    Bind(BindRequest),
    Lookup(LookupRequest),
    Owned(OwnedRequest),
    Tombstone(TombstoneRequest),
    Claim(ClaimRequest),
    Renew(LeaseRequest),
    Reschedule(LeaseRequest),
    Complete(CompleteRequest),
    BeginRelated(BeginRelatedRequest),
    BindRelated(BindRelatedRequest),
    ReserveRelations(ReserveRelationsRequest),
    CommitRelations(CommitRelationsRequest),
    LookupRelations(LookupRelationsRequest),
    ResolveReservation(ResolveReservationRequest),
    BindObservedArtifacts(BindObservedArtifactsRequest),
    ReserveDelete(ObjectDeletionRequest),
    CommitDelete(ObjectDeletionRequest),
    SignedSipIntake(SignedSipIntakeRequest),
    MintCredential(MintCredentialRequest),
    RedeemCredential(RedeemCredentialRequest),
    CreateLiveSession(CreateLiveSessionRequest),
    AppendLiveUsage(AppendLiveUsageRequest),
    CheckpointLiveSession(CheckpointLiveSessionRequest),
    ClaimLiveSessions(ClaimLiveSessionsRequest),
    CompleteTyped(TypedCompleteRequest),
    Reconciliation(ReconciliationRequest),
}

pub(crate) fn unavailable<T>(_error: T) -> ApiError {
    // 数据库错误可能携带内部值，此处不打印查询、摘要或原始文档。
    tracing::warn!("native object authority is unavailable");
    ApiError::unavailable()
}

fn invalid(path: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: "原生对象参数不合法".to_string(),
    }])
}

fn conflict() -> ApiError {
    ApiError::conflict("native_object_conflict", "原生对象操作与已有状态冲突")
}

fn text_ok(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

struct Principal {
    tenant: TenantRow,
    key: KeyRow,
    full: bool,
}

async fn principal(conn: &mut PgConnection, id: Uuid) -> Result<Principal, ApiError> {
    let tenant: Option<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM tenant_keys WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(unavailable)?;
    let tenant = tenant.ok_or_else(ApiError::not_found)?;
    // 所有密钥和成员写入先锁其租户，分享该锁就能稳定这次授权决策。
    sqlx::query("SELECT id FROM tenants WHERE id = $1 FOR SHARE")
        .bind(tenant)
        .fetch_one(&mut *conn)
        .await
        .map_err(unavailable)?;
    let tenant = tenants::find(&mut *conn, tenant).await?;
    let key = keys::live_key(&mut *conn, id, Some(tenant.id), None).await?;
    let active = match key.member_id {
        None => true,
        Some(member) => sqlx::query_scalar::<_, bool>(
            "SELECT NOT disabled AND removed_at IS NULL \
             AND role IN ('owner', 'admin', 'member') FROM members \
             WHERE id = $1 AND tenant_id = $2",
        )
        .bind(member)
        .bind(tenant.id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?
        .unwrap_or(false),
    };
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await
        .map_err(unavailable)?;
    if keys::effective_disabled(&tenant, &key, active) || key.expires_at.is_some_and(|at| at <= now)
    {
        return Err(ApiError::not_found());
    }
    if let Some(group) = &key.group {
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM provider_groups WHERE id = $1 AND retired_at IS NULL)",
        )
        .bind(group.id)
        .fetch_one(&mut *conn)
        .await
        .map_err(unavailable)?;
        if !live {
            return Err(ApiError::not_found());
        }
    }
    let access = access::key_access(&mut *conn, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Principal {
        tenant,
        key,
        full: access.visibility == Visibility::Full,
    })
}

fn owns(principal: &Principal, row: &ObjectRow) -> bool {
    row.tenant_id == principal.tenant.id
        && (principal.full
            || match principal.key.member_id {
                Some(member) => row.member_id == Some(member),
                None => row.api_key_id == principal.key.id,
            })
}

fn endpoint_permitted(
    key: &ApiKey,
    kind: NativeObjectKind,
    consumer_family: Option<EndpointFamily>,
) -> bool {
    key.can_call(consumer_family.unwrap_or_else(|| kind.endpoint_family()))
}

// 通配模型保存请求时的具体名称；普通模型仍跟随当前资源重命名。
fn requested_authority_name<'a>(model: &'a Model, submitted: &'a BeginRequest) -> Option<&'a str> {
    if model.display_name.contains('*') {
        aisix_core::wildcard::wildcard_matches(&model.display_name, &submitted.requested_model)
            .then_some(submitted.requested_model.as_str())
    } else {
        Some(model.display_name.as_str())
    }
}

struct OriginResources {
    models: HashMap<Uuid, Model>,
    providers: HashMap<Uuid, ProviderKey>,
}

async fn load_origin_resources(
    conn: &mut PgConnection,
    state: &AppState,
    model_ids: &[Uuid],
    provider_ids: &[Uuid],
    check_models: bool,
) -> Result<OriginResources, ApiError> {
    #[cfg(test)]
    begin_perf_tests::resource_load();
    let rows: Vec<(String, Uuid, Value)> = sqlx::query_as(&format!(
        "SELECT kind, id, doc FROM gw_resources WHERE NOT deleted AND \
         ((kind = 'models' AND id = ANY($1)) OR (kind = 'provider_keys' AND id = ANY($2))) \
         ORDER BY kind, id{}",
        if check_models { " FOR SHARE" } else { "" },
    ))
    .bind(model_ids)
    .bind(provider_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(unavailable)?;
    let mut resources = OriginResources {
        models: HashMap::new(),
        providers: HashMap::new(),
    };
    for (kind, id, mut doc) in rows {
        if kind == "models" && check_models {
            resources
                .models
                .insert(id, serde_json::from_value(doc).map_err(unavailable)?);
        } else if kind == "provider_keys" {
            secrets::unseal(&state.cfg.master_key, Kind::ProviderKeys, id, &mut doc)
                .map_err(unavailable)?;
            resources
                .providers
                .insert(id, serde_json::from_value(doc).map_err(unavailable)?);
        }
    }
    Ok(resources)
}

async fn origin_for_consumer(
    conn: &mut PgConnection,
    state: &AppState,
    principal: Option<&Principal>,
    submission: &BeginRequest,
    consumer_family: Option<EndpointFamily>,
) -> Result<(), ApiError> {
    let resources = load_origin_resources(
        &mut *conn,
        state,
        &[submission.origin.model_id, submission.requested_model_id],
        &[submission.origin.provider_key_id],
        principal.is_some(),
    )
    .await?;
    origin_for_consumer_loaded(
        conn,
        state,
        principal,
        submission,
        consumer_family,
        &resources,
        false,
    )
    .await
}

async fn origin_for_consumer_loaded(
    conn: &mut PgConnection,
    state: &AppState,
    principal: Option<&Principal>,
    submission: &BeginRequest,
    consumer_family: Option<EndpointFamily>,
    resources: &OriginResources,
    source_checked: bool,
) -> Result<(), ApiError> {
    let model = principal
        .map(|_| {
            resources
                .models
                .get(&submission.origin.model_id)
                .ok_or_else(ApiError::not_found)
        })
        .transpose()?;
    let requested = principal
        .map(|_| {
            resources
                .models
                .get(&submission.requested_model_id)
                .ok_or_else(ApiError::not_found)
        })
        .transpose()?;
    if model.as_ref().is_some_and(|model| {
        model.provider_key_id.as_deref()
            != Some(submission.origin.provider_key_id.to_string().as_str())
    }) {
        return Err(ApiError::conflict(
            "native_origin_changed",
            "原生对象所属账号已变化",
        ));
    }
    let provider = resources
        .providers
        .get(&submission.origin.provider_key_id)
        .ok_or_else(ApiError::not_found)?;
    let id_key =
        aisix_core::native_objects::platform_id_key_from_secret(&state.pki.platform_id_key_hex());
    if !source_checked
        && submission.origin.surface.is_some_and(|surface| {
            surface != native_origin_surface(submission.protocol, submission.kind)
        })
    {
        let source: Option<Uuid> = sqlx::query_scalar("SELECT source_record_id FROM native_operation_origin_sources WHERE environment_id=$1 AND operation_id=$2")
            .bind(state.env.env_id).bind(submission.operation_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
        let source_id = source
            .or(submission.parent_id)
            .ok_or_else(|| invalid("/origin/surface"))?;
        let source: ObjectRow =
            sqlx::query_as("SELECT * FROM native_objects WHERE environment_id=$1 AND id=$2")
                .bind(state.env.env_id)
                .bind(source_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(unavailable)?
                .ok_or_else(ApiError::not_found)?;
        let source_op = operation(&mut *conn, state, source.operation_id, false).await?;
        let accepted_source = submission_from_source(&source_op)?;
        let source_surface = accepted_source
            .origin
            .surface
            .unwrap_or(native_origin_surface(
                accepted_source.protocol,
                accepted_source.kind,
            ));
        if Some(source_surface) != submission.origin.surface
            || source.provider_key_id != submission.origin.provider_key_id
            || source.boundary != submission.origin.boundary.as_str()
        {
            return Err(invalid("/origin/surface"));
        }
    }
    let digest = configured_native_origin_at(
        &id_key,
        submission.origin.provider_key_id,
        provider,
        submission.protocol,
        submission.kind,
        submission.origin.surface,
    )
    .map_err(|_| ApiError::conflict("native_origin_changed", "无法确认原生对象所属账号"))?;
    if digest != submission.origin.boundary {
        return Err(ApiError::conflict(
            "native_origin_changed",
            "原生对象所属账号已变化",
        ));
    }
    if let Some(principal) = principal {
        let requested_name = requested
            .as_ref()
            .and_then(|model| requested_authority_name(model, submission));
        let allowed = requested_name.is_some()
            && match keys::reach(&principal.tenant, &principal.key) {
                KeyReach::Ids(ids) => ids.contains(&submission.requested_model_id),
                names => requested_name.is_some_and(|name| names.names(name)),
            };
        let groups = keys::effective_groups(&principal.tenant, &principal.key);
        let key_doc: ApiKey =
            serde_json::from_value(keys::build_key_doc(&principal.tenant, &principal.key, true))
                .map_err(unavailable)?;
        if !allowed
            || !endpoint_permitted(&key_doc, submission.kind, consumer_family)
            || (!groups.is_empty() && !groups.iter().any(|g| provider.groups.contains(g)))
        {
            return Err(ApiError::not_found());
        }
    }
    Ok(())
}

async fn origin(
    conn: &mut PgConnection,
    state: &AppState,
    principal: Option<&Principal>,
    submission: &BeginRequest,
) -> Result<(), ApiError> {
    origin_for_consumer(conn, state, principal, submission, None).await
}

#[derive(sqlx::FromRow)]
struct OperationRow {
    visibility_full: bool,
    dp_id: Uuid,
    tenant_id: Uuid,
    api_key_id: Uuid,
    member_id: Option<Uuid>,
    submission: Value,
    state: String,
}

async fn operation(
    conn: &mut PgConnection,
    state: &AppState,
    id: Uuid,
    lock: bool,
) -> Result<OperationRow, ApiError> {
    sqlx::query_as(&format!(
        "SELECT visibility_full, dp_id, tenant_id, api_key_id, member_id, submission, state FROM native_operations \
         WHERE environment_id = $1 AND operation_id = $2{}",
        if lock { " FOR NO KEY UPDATE" } else { "" },
    ))
    .bind(state.env.env_id)
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(unavailable)?
    .ok_or_else(ApiError::not_found)
}

fn submission_from_source(row: &OperationRow) -> Result<BeginRequest, ApiError> {
    serde_json::from_value(row.submission.clone()).map_err(unavailable)
}

fn submission(row: &OperationRow) -> Result<BeginRequest, ApiError> {
    submission_from_source(row)
}

#[derive(sqlx::FromRow)]
struct ObjectRow {
    id: Uuid,
    environment_id: Uuid,
    operation_id: Uuid,
    tenant_id: Uuid,
    api_key_id: Uuid,
    member_id: Option<Uuid>,
    protocol: String,
    kind: String,
    model_id: Uuid,
    provider_key_id: Uuid,
    boundary: String,
    raw_id: String,
    parent_id: Option<Uuid>,
    version: i64,
    deleted_at: Option<DateTime<Utc>>,
    lease_dp_id: Option<Uuid>,
    lease_expires_at: Option<DateTime<Utc>>,
    lease_generation: i64,
    completed_at: Option<DateTime<Utc>>,
    membership_epoch: i64,
}

impl ObjectRow {
    fn view(&self, submitted: &BeginRequest) -> Result<NativeRecord, ApiError> {
        Ok(NativeRecord {
            upstream_input_text: submitted.upstream_input_text,
            input_file_id: submitted.input_file_id.clone(),
            record_id: self.id,
            api_key_id: Some(self.api_key_id),
            entries: submitted.entries.clone(),
            observation_headers: submitted.observation_headers.clone(),
            completed: self.completed_at.is_some(),
            operation_id: Some(self.operation_id),
            requested_model_id: Some(submitted.requested_model_id),
            requested_model: submitted.requested_model.clone(),
            upstream_model: submitted.upstream_model.clone(),
            upstream_provider: submitted.upstream_provider.clone(),
            operation: submitted.operation.clone(),
            request_id: submitted.request_id.clone(),
            requested_service_tier: submitted.requested_service_tier.clone(),
            origin: Some(OriginBinding {
                surface: submitted.origin.surface,
                model_id: self.model_id,
                provider_key_id: self.provider_key_id,
                boundary: serde_json::from_value(json!(self.boundary)).map_err(unavailable)?,
            }),
            protocol: Some(serde_json::from_value(json!(self.protocol)).map_err(unavailable)?),
            kind: Some(serde_json::from_value(json!(self.kind)).map_err(unavailable)?),
            raw_id: self.raw_id.clone(),
            parent_id: self.parent_id,
            parent_raw_id: None,
            lifecycle_context: None,
            metering_state: None,
            reconciliation_reason: None,
            version: self.version,
            lease_generation: (self.lease_dp_id.is_some()).then_some(self.lease_generation),
            lease_expires_at: self.lease_expires_at,
            deleted: self.deleted_at.is_some(),
        })
    }
}

async fn object(
    conn: &mut PgConnection,
    state: &AppState,
    id: Uuid,
) -> Result<ObjectRow, ApiError> {
    sqlx::query_as("SELECT * FROM native_objects WHERE environment_id = $1 AND id = $2 FOR UPDATE")
        .bind(state.env.env_id)
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(unavailable)?
        .ok_or_else(ApiError::not_found)
}

async fn authorized_for_consumer(
    conn: &mut PgConnection,
    state: &AppState,
    principal: &Principal,
    row: &ObjectRow,
    consumer_family: Option<EndpointFamily>,
) -> Result<(), ApiError> {
    if row.environment_id != state.env.env_id || !owns(principal, row) || row.deleted_at.is_some() {
        return Err(ApiError::not_found());
    }
    let op = operation(&mut *conn, state, row.operation_id, false).await?;
    let submitted = submission(&op)?;
    #[derive(sqlx::FromRow)]
    struct BatchParent {
        #[sqlx(flatten)]
        row: ObjectRow,
        batch_submission: Value,
    }
    // File 只保留直接 Batch 的既有依赖；其他来源不递归形成当前权限闭包。
    let batch_parent: Option<BatchParent> = if row.kind == "file" {
        if let Some(parent_id) = row.parent_id {
            sqlx::query_as(
                "SELECT parent.*, operation.submission AS batch_submission \
                 FROM native_objects parent JOIN native_operations operation \
                 ON operation.environment_id=parent.environment_id AND operation.operation_id=parent.operation_id \
                 WHERE parent.id=$1 AND parent.kind='message_batch'",
            ).bind(parent_id).fetch_optional(&mut *conn).await.map_err(unavailable)?
        } else {
            None
        }
    } else {
        None
    };
    let parent_submission = if let Some(parent) = &batch_parent {
        if parent.row.environment_id != state.env.env_id
            || !owns(principal, &parent.row)
            || parent.row.deleted_at.is_some()
            || parent.row.protocol != row.protocol
            || parent.row.provider_key_id != row.provider_key_id
            || parent.row.boundary != row.boundary
        {
            return Err(ApiError::not_found());
        }
        Some(
            serde_json::from_value::<BeginRequest>(parent.batch_submission.clone())
                .map_err(unavailable)?,
        )
    } else {
        None
    };
    let submissions = [Some(&submitted), parent_submission.as_ref()];
    let mut model_ids = BTreeSet::new();
    let mut provider_ids = BTreeSet::new();
    for accepted in submissions.iter().flatten() {
        model_ids.insert(accepted.origin.model_id);
        model_ids.insert(accepted.requested_model_id);
        provider_ids.insert(accepted.origin.provider_key_id);
        for entry in &accepted.entries {
            model_ids.insert(entry.model_id);
            model_ids.insert(entry.requested_model_id);
        }
    }
    let model_ids: Vec<_> = model_ids.into_iter().collect();
    let provider_ids: Vec<_> = provider_ids.into_iter().collect();
    let resources =
        load_origin_resources(&mut *conn, state, &model_ids, &provider_ids, true).await?;
    for accepted in submissions.into_iter().flatten() {
        origin_for_consumer_loaded(
            &mut *conn,
            state,
            Some(principal),
            accepted,
            consumer_family,
            &resources,
            false,
        )
        .await?;
        // 同一提交内按实际与请求身份去重；不同名称保留各自检查，不共享跨请求缓存。
        let mut seen = BTreeSet::from([(
            accepted.origin.model_id,
            accepted.requested_model_id,
            accepted.requested_model.as_str(),
        )]);
        let mut per_entry: Option<BeginRequest> = None;
        for entry in &accepted.entries {
            // 新条目用自己的请求名；旧空值沿用既有提交名，不补造历史 alias。
            let requested_name = if entry.requested_model.is_empty() {
                accepted.requested_model.as_str()
            } else {
                entry.requested_model.as_str()
            };
            if !seen.insert((entry.model_id, entry.requested_model_id, requested_name)) {
                continue;
            }
            let checked = per_entry.get_or_insert_with(|| {
                let mut copy = (*accepted).clone();
                copy.entries.clear();
                copy
            });
            checked.origin.model_id = entry.model_id;
            checked.requested_model_id = entry.requested_model_id;
            checked.requested_model.clear();
            checked.requested_model.push_str(requested_name);
            // 每项只改模型身份和请求名；surface 来源已在同一提交的检查中验证。
            origin_for_consumer_loaded(
                &mut *conn,
                state,
                Some(principal),
                checked,
                consumer_family,
                &resources,
                true,
            )
            .await?;
        }
    }
    Ok(())
}

async fn authorized(
    conn: &mut PgConnection,
    state: &AppState,
    principal: &Principal,
    row: &ObjectRow,
) -> Result<(), ApiError> {
    authorized_for_consumer(conn, state, principal, row, None).await
}

async fn begin(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: BeginRequest,
) -> Result<NativeReply, ApiError> {
    begin_with_locked_parent(conn, state, dp, body, None).await
}

async fn begin_with_locked_parent(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    mut body: BeginRequest,
    locked_parent: Option<&ObjectRow>,
) -> Result<NativeReply, ApiError> {
    // 新对象明确保存所选面；已发布种类缺省值保持旧线协议，供滚动升级重放。
    if body.origin.surface.is_none()
        && !matches!(
            body.kind,
            NativeObjectKind::Response
                | NativeObjectKind::ResponseItem
                | NativeObjectKind::ResponseContainer
                | NativeObjectKind::File
                | NativeObjectKind::MessageBatch
                | NativeObjectKind::ChatCompletion
                | NativeObjectKind::Upload
        )
    {
        body.origin.surface = Some(native_origin_surface(body.protocol, body.kind));
    }
    if !text_ok(&body.request_id, 256)
        || !text_ok(&body.requested_model, 256)
        || !text_ok(&body.upstream_model, 256)
        || !text_ok(&body.operation, 64)
        || body
            .requested_service_tier
            .as_ref()
            .is_some_and(|tier| !text_ok(tier, 64))
        || body.entries.len() > MAX_ENTRIES
        || body
            .upstream_input_text
            .as_ref()
            .is_some_and(|text| !text.is_valid())
    {
        return Err(invalid("/submission"));
    }
    if body.kind == NativeObjectKind::MessageBatch && body.entries.is_empty() {
        return Err(invalid("/entries"));
    }
    if !matches!(
        body.kind,
        NativeObjectKind::MessageBatch
            | NativeObjectKind::Eval
            | NativeObjectKind::EvalRun
            | NativeObjectKind::RealtimeCall
    ) && !(body.protocol == NativeProtocol::Openai && body.kind == NativeObjectKind::File)
        && !body.entries.is_empty()
    {
        return Err(invalid("/entries"));
    }
    if body.observation_headers.iter().any(|(name, value)| {
        !matches!(name.as_str(), "anthropic-version" | "anthropic-beta")
            || value.is_empty()
            || value.len() > 8192
            || value.chars().any(char::is_control)
    }) {
        return Err(invalid("/observation_headers"));
    }
    let batch_input = body.protocol == NativeProtocol::Openai
        && body.kind == NativeObjectKind::File
        && body.operation == "batch_input"
        && body.input_file_id.is_some()
        && body.parent_id.is_some()
        && !body.entries.is_empty();
    let consumer_family = batch_input.then_some(EndpointFamily::Batches);
    let principal = principal(&mut *conn, body.api_key_id).await?;
    // 同一事务按既有顺序锁住根和每项的两种模型身份，不按 entry 重读账号。
    let model_ids: Vec<_> = std::iter::once(body.origin.model_id)
        .chain(std::iter::once(body.requested_model_id))
        .chain(
            body.entries
                .iter()
                .flat_map(|entry| [entry.model_id, entry.requested_model_id]),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let resources = load_origin_resources(
        &mut *conn,
        state,
        &model_ids,
        &[body.origin.provider_key_id],
        true,
    )
    .await?;
    origin_for_consumer_loaded(
        &mut *conn,
        state,
        Some(&principal),
        &body,
        consumer_family,
        &resources,
        false,
    )
    .await?;
    let model = resources
        .models
        .get(&body.origin.model_id)
        .ok_or_else(ApiError::not_found)?;
    if model.model_name.as_deref() != Some(body.upstream_model.as_str()) {
        return Err(invalid("/upstream_model"));
    }
    body.upstream_provider = model.provider.clone().unwrap_or_default();
    let mut entries = BTreeSet::new();
    #[cfg(test)]
    begin_perf_tests::working_copy(&body);
    let mut base_submission = body.clone();
    base_submission.entries.clear();
    base_submission.usage_context = None;
    for entry in &mut body.entries {
        if matches!(
            body.kind,
            NativeObjectKind::Eval | NativeObjectKind::EvalRun
        ) != (entry.endpoint == "/v1/evals")
        {
            return Err(invalid("/entries/endpoint"));
        }
        if (body.kind == NativeObjectKind::RealtimeCall)
            != (entry.endpoint == "/v1/audio/transcriptions")
        {
            return Err(invalid("/entries/endpoint"));
        }
        if !text_ok(&entry.entry_id, 256)
            || entry
                .upstream_input_text
                .as_ref()
                .is_some_and(|text| !text.is_valid())
            || (body.protocol == NativeProtocol::Openai
                && !matches!(
                    entry.endpoint.as_str(),
                    "/v1/chat/completions"
                        | "/v1/evals"
                        | "/v1/audio/transcriptions"
                        | "/v1/completions"
                        | "/v1/responses"
                        | "/v1/embeddings"
                        | "/v1/moderations"
                        | "/v1/images/generations"
                        | "/v1/images/edits"
                ))
            || !text_ok(&entry.upstream_model, 256)
            || !entries.insert(&entry.entry_id)
            || entry
                .requested_service_tier
                .as_ref()
                .is_some_and(|tier| !text_ok(tier, 64))
        {
            return Err(invalid("/entries"));
        }
        if !entry.requested_model.is_empty() && !text_ok(&entry.requested_model, 256) {
            return Err(invalid("/entries/requested_model"));
        }
        base_submission.origin.model_id = entry.model_id;
        base_submission.requested_model_id = entry.requested_model_id;
        base_submission
            .upstream_model
            .clone_from(&entry.upstream_model);
        // 旧空值仍沿用根请求名；非空名必须按本项的 wildcard 匹配，不能借根名授权。
        base_submission
            .requested_model
            .clone_from(if entry.requested_model.is_empty() {
                &body.requested_model
            } else {
                &entry.requested_model
            });
        // 来源的 surface/parent/账号边界沿用已检查的根；模型、别名和组仍逐项检查。
        origin_for_consumer_loaded(
            &mut *conn,
            state,
            Some(&principal),
            &base_submission,
            consumer_family,
            &resources,
            true,
        )
        .await?;
        let model = resources
            .models
            .get(&entry.model_id)
            .ok_or_else(ApiError::not_found)?;
        if model.model_name.as_deref() != Some(entry.upstream_model.as_str()) {
            return Err(invalid("/entries/upstream_model"));
        }
        let current_name = &resources
            .models
            .get(&entry.requested_model_id)
            .ok_or_else(ApiError::not_found)?
            .display_name;
        if current_name.is_empty() {
            return Err(invalid("/entries/requested_model"));
        }
        if entry.requested_model.is_empty() {
            entry.requested_model.clone_from(current_name);
        }
        entry.upstream_provider = model.provider.clone().unwrap_or_default();
    }
    if let Some(raw_id) = &body.input_file_id {
        if body.protocol != NativeProtocol::Openai
            || (body.kind != NativeObjectKind::MessageBatch && !batch_input)
        {
            return Err(invalid("/input_file_id"));
        }
        let source = lookup(
            &mut *conn,
            state,
            LookupRequest {
                consumer_family: Some(EndpointFamily::Batches),
                api_key_id: body.api_key_id,
                locator: ObjectLocator {
                    protocol: NativeProtocol::Openai,
                    kind: NativeObjectKind::File,
                    raw_id: raw_id.clone(),
                    origin: Some(body.origin.clone()),
                    parent_id: None,
                },
            },
        )
        .await?;
        if source.record.as_ref().is_none_or(|record| {
            record.origin.as_ref() != Some(&body.origin)
                || (batch_input && body.parent_id != Some(record.record_id))
        }) {
            return Err(invalid("/input_file_id"));
        }
    }
    let wire = serde_json::to_value(&body).map_err(unavailable)?;
    sqlx::query(
        "INSERT INTO native_operations (environment_id, operation_id, dp_id, tenant_id, \
         api_key_id, member_id, visibility_full, submission) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
         ON CONFLICT (environment_id, operation_id) DO NOTHING",
    )
    .bind(state.env.env_id)
    .bind(body.operation_id)
    .bind(dp.dp_id)
    .bind(principal.tenant.id)
    .bind(principal.key.id)
    .bind(principal.key.member_id)
    .bind(principal.full)
    .bind(&wire)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    let stored = operation(&mut *conn, state, body.operation_id, true).await?;
    if stored.tenant_id != principal.tenant.id
        || stored.api_key_id != principal.key.id
        || submission(&stored)? != body
    {
        return Err(conflict());
    }
    if let Some(parent) = body.parent_id {
        let loaded_parent;
        let parent = match locked_parent.filter(|row| row.id == parent) {
            Some(parent) => parent,
            None => {
                loaded_parent = object(&mut *conn, state, parent).await?;
                &loaded_parent
            }
        };
        foundation::assert_unfenced(&mut *conn, state, parent.id, Some(body.operation_id)).await?;
        let parent_consumer = if body.protocol == NativeProtocol::Openai
            && body.kind == NativeObjectKind::Response
            && parent.protocol == "openai"
            && parent.kind == "conversation"
        {
            Some(EndpointFamily::Responses)
        } else {
            consumer_family
        };
        authorized_for_consumer(&mut *conn, state, &principal, parent, parent_consumer).await?;
        if parent.boundary != body.origin.boundary.as_str() {
            return Err(invalid("/parent_id"));
        }
    }
    let mut reply = NativeReply::new(&stored.state);
    reply.operation_id = Some(body.operation_id);
    reply.origin = Some(body.origin);
    Ok(reply)
}

async fn bind(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: BindRequest,
) -> Result<NativeReply, ApiError> {
    if !text_ok(&body.raw_id, 512) {
        return Err(invalid("/raw_id"));
    }
    if body.objects.len() > MAX_ENTRIES
        || body.objects.iter().any(|object| {
            !matches!(
                object.kind,
                NativeObjectKind::ResponseItem | NativeObjectKind::ResponseContainer
            ) || !text_ok(&object.raw_id, 512)
        })
    {
        return Err(invalid("/objects"));
    }
    let op = operation(&mut *conn, state, body.operation_id, true).await?;
    // 上游已受理的结果必须落到原提交；撤权、换模型或换凭据不能抹掉待结算身份。
    let submitted = submission(&op)?;
    // 正式 C 与本次已有 R 必须在任何子对象或 identity 锁前按共同顺序取得。
    let ordered_parent = submitted.protocol == NativeProtocol::Openai
        && submitted.kind == NativeObjectKind::Response
        && submitted.parent_id.is_some();
    let mut locked_objects = if ordered_parent {
        let mut ids = Vec::from_iter(submitted.parent_id);
        ids.extend(body.existing_record_id);
        foundation::lock_command_objects(&mut *conn, state, &ids, Some(body.operation_id)).await?
    } else {
        std::collections::BTreeMap::new()
    };
    let mut refresh_principal = None;
    let mut locked_root = None;
    if let Some(record_id) = body.existing_record_id {
        let existing = match locked_objects.remove(&record_id) {
            Some(existing) => existing,
            None => object(&mut *conn, state, record_id).await?,
        };
        if op.state != "bound"
            || existing.operation_id != body.operation_id
            || existing.raw_id != body.raw_id
            || existing.protocol != "openai"
            || existing.kind != "response"
            || submitted.protocol != NativeProtocol::Openai
            || submitted.kind != NativeObjectKind::Response
            || existing.model_id != submitted.origin.model_id
            || existing.provider_key_id != submitted.origin.provider_key_id
            || existing.boundary != submitted.origin.boundary.as_str()
        {
            return Err(ApiError::not_found());
        }
        let principal = principal(&mut *conn, body.api_key_id).await?;
        authorized(&mut *conn, state, &principal, &existing).await?;
        refresh_principal = Some(principal);
        if ordered_parent {
            locked_root = Some(existing);
        }
    } else if op.dp_id != dp.dp_id || op.api_key_id != body.api_key_id {
        return Err(ApiError::not_found());
    }
    if !body.objects.is_empty()
        && (submitted.protocol != NativeProtocol::Openai
            || submitted.kind != NativeObjectKind::Response)
    {
        return Err(invalid("/objects"));
    }
    let mut conversation_writable = false;
    let conversation = if let Some(parent) = submitted.parent_id {
        let parent = match locked_objects.remove(&parent) {
            Some(parent) => parent,
            None => object(&mut *conn, state, parent).await?,
        };
        let is_conversation = submitted.protocol == NativeProtocol::Openai
            && submitted.kind == NativeObjectKind::Response
            && parent.protocol == "openai"
            && parent.kind == "conversation";
        let fenced = if is_conversation {
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM native_parent_fences WHERE environment_id=$1 AND parent_id=$2 AND state IN ('held','uncertain') AND operation_id<>$3)")
                .bind(state.env.env_id).bind(parent.id).bind(body.operation_id)
                .fetch_one(&mut *conn).await.map_err(unavailable)?
        } else {
            foundation::assert_unfenced(&mut *conn, state, parent.id, Some(body.operation_id))
                .await?;
            false
        };
        if parent.tenant_id != op.tenant_id || parent.boundary != submitted.origin.boundary.as_str()
        {
            return Err(conflict());
        }
        // 墓碑只提供已接受产物的不可变 owner；管理栅栏不撤销已经发生的 R/I。
        conversation_writable = is_conversation && parent.deleted_at.is_none() && !fenced;
        is_conversation.then_some(parent)
    } else {
        None
    };
    sqlx::query(
        "INSERT INTO native_objects (id,environment_id,operation_id,tenant_id,api_key_id, \
         member_id,protocol,kind,model_id,provider_key_id,boundary,raw_id,parent_id,pending, \
         provider_created_at,provider_expires_at) VALUES \
         ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) ON CONFLICT DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(state.env.env_id)
    .bind(body.operation_id)
    .bind(op.tenant_id)
    .bind(op.api_key_id)
    .bind(op.member_id)
    .bind(submitted.protocol.as_str())
    .bind(submitted.kind.as_str())
    .bind(submitted.origin.model_id)
    .bind(submitted.origin.provider_key_id)
    .bind(submitted.origin.boundary.as_str())
    .bind(&body.raw_id)
    .bind(submitted.parent_id)
    .bind(submitted.asynchronous)
    .bind(body.provider_created_at)
    .bind(body.provider_expires_at)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    let row = match locked_root.or_else(|| {
        locked_objects
            .into_values()
            .find(|row| row.operation_id == body.operation_id)
    }) {
        Some(row) => Some(row),
        None => sqlx::query_as(
            "SELECT * FROM native_objects WHERE environment_id = $1 AND operation_id = $2",
        )
        .bind(state.env.env_id)
        .bind(body.operation_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?,
    };
    let row = row.ok_or_else(conflict)?;
    if row.raw_id != body.raw_id {
        return Err(conflict());
    }
    sqlx::query("UPDATE native_operations SET state = 'bound' WHERE environment_id = $1 AND operation_id = $2")
        .bind(state.env.env_id).bind(body.operation_id).execute(&mut *conn).await.map_err(unavailable)?;
    // 子对象从原始提交派生，重试只确认既有归属，绝不把其他租户的裸 ID 收为己有。
    let mut seen = BTreeSet::new();
    let mut children = Vec::new();
    let mut objects = body.objects;
    objects.sort_by(|a, b| (a.kind.as_str(), &a.raw_id).cmp(&(b.kind.as_str(), &b.raw_id)));
    #[derive(sqlx::FromRow)]
    struct ExistingConversationItem {
        #[sqlx(flatten)]
        row: ObjectRow,
        previous: Value,
        accepted_output: bool,
        source_r: bool,
    }
    for object in objects {
        if !seen.insert((object.kind.as_str(), object.raw_id.clone())) {
            continue;
        }
        // 不同 Response 可以复用同一容器；缺行时也锁住身份，避免并发 INSERT 冲突回滚已付费结果。
        let lock_identity = serde_json::to_string(&(
            state.env.env_id,
            "openai",
            object.kind.as_str(),
            &row.boundary,
            &object.raw_id,
        ))
        .map_err(unavailable)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(lock_identity)
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
        let (existing, previous, accepted_output, source_r) = if conversation.is_some()
            && object.kind == NativeObjectKind::ResponseItem
        {
            let found: Option<ExistingConversationItem> = sqlx::query_as(
                "SELECT item.*, operation.submission AS previous, \
                 EXISTS(SELECT 1 FROM native_object_memberships membership WHERE membership.environment_id=item.environment_id AND membership.parent_id=$5 AND membership.child_id=item.id AND membership.relation='response_output' AND membership.created_operation_id=$6) AS accepted_output, \
                 EXISTS(SELECT 1 FROM native_operation_origin_sources source WHERE source.environment_id=item.environment_id AND source.operation_id=item.operation_id AND source.source_record_id=$5) AS source_r \
                 FROM native_objects item JOIN native_operations operation ON operation.environment_id=item.environment_id AND operation.operation_id=item.operation_id \
                 WHERE item.environment_id=$1 AND item.protocol='openai' AND item.kind=$2 AND item.boundary=$3 AND item.raw_id=$4 FOR UPDATE OF item",
            )
            .bind(state.env.env_id).bind(object.kind.as_str()).bind(&row.boundary).bind(&object.raw_id)
            .bind(row.id).bind(body.operation_id)
            .fetch_optional(&mut *conn).await.map_err(unavailable)?;
            match found {
                Some(found) => (
                    Some(found.row),
                    Some(found.previous),
                    found.accepted_output,
                    found.source_r,
                ),
                None => (None, None, false, false),
            }
        } else {
            let found: Option<ObjectRow> = sqlx::query_as(
                "SELECT * FROM native_objects WHERE environment_id=$1 AND protocol='openai' AND kind=$2 AND boundary=$3 AND raw_id=$4 FOR UPDATE",
            )
            .bind(state.env.env_id).bind(object.kind.as_str()).bind(&row.boundary).bind(&object.raw_id)
            .fetch_optional(&mut *conn).await.map_err(unavailable)?;
            (found, None, false, false)
        };
        if let Some(existing) = existing {
            if existing.tenant_id != row.tenant_id
                || existing.provider_key_id != row.provider_key_id
                || existing.boundary != row.boundary
                || existing.deleted_at.is_some()
            {
                return Err(conflict());
            }
            let previous: Option<BeginRequest> = previous
                .map(|value| serde_json::from_value(value).map_err(unavailable))
                .transpose()?;
            let accepted_item = existing.parent_id == Some(row.id) && accepted_output;
            if let Some(principal) = &refresh_principal {
                // 仅本 R 已保存的完整派生模板可复用刚通过的当前 R 权限；外来对象仍逐项授权。
                let own_output = accepted_item
                    && source_r
                    && existing.model_id == row.model_id
                    && previous.as_ref().is_some_and(|previous| {
                        let mut expected = submitted.clone();
                        expected.operation_id = existing.operation_id;
                        expected.kind = NativeObjectKind::ResponseItem;
                        expected.parent_id = Some(row.id);
                        expected.asynchronous = false;
                        expected.operation = NativeObjectKind::ResponseItem.as_str().into();
                        previous == &expected && previous.origin.surface == expected.origin.surface
                    });
                if !own_output {
                    authorized(&mut *conn, state, principal, &existing).await?;
                }
            } else if !op.visibility_full
                && existing.api_key_id != op.api_key_id
                && !(op.member_id.is_some() && existing.member_id == op.member_id)
            {
                // 初次接受后的绑定只用准入时的可信身份；撤权不能抹掉已发生的推理。
                if !accepted_item {
                    return Err(conflict());
                }
            }
            let previous = match previous {
                Some(previous) => previous,
                None => {
                    submission(&operation(&mut *conn, state, existing.operation_id, false).await?)?
                }
            };
            insert_membership(
                &mut *conn,
                state,
                &row,
                &existing,
                NativeRelation::ResponseOutput,
                body.operation_id,
            )
            .await?;
            if object.kind == NativeObjectKind::ResponseItem && conversation_writable {
                if let Some(parent) = &conversation {
                    bind_response_conversation_item(
                        &mut *conn,
                        state,
                        parent,
                        &existing,
                        body.operation_id,
                        accepted_output,
                    )
                    .await?;
                }
            }
            children.push(existing.view(&previous)?);
            continue;
        }
        let operation_id = Uuid::new_v4();
        let mut derived = submitted.clone();
        derived.operation_id = operation_id;
        derived.kind = object.kind;
        derived.parent_id = Some(row.id);
        derived.asynchronous = false;
        derived.operation = object.kind.as_str().into();
        // 会话只提供新项目的 owner；执行、可见性快照和物理来源仍属于本次 Response。
        let item_owner = conversation
            .as_ref()
            .filter(|_| object.kind == NativeObjectKind::ResponseItem);
        let (owner_tenant, owner_key, owner_member) = item_owner
            .map_or((op.tenant_id, op.api_key_id, op.member_id), |parent| {
                (parent.tenant_id, parent.api_key_id, parent.member_id)
            });
        let child: ObjectRow = if item_owner.is_some() {
            sqlx::query_as(
                "WITH made_operation AS (\
                 INSERT INTO native_operations(environment_id,operation_id,dp_id,tenant_id,api_key_id,member_id,visibility_full,submission,state) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'bound') RETURNING operation_id\
                 ), made_source AS (\
                 INSERT INTO native_operation_origin_sources(environment_id,operation_id,source_record_id) SELECT $1,operation_id,$9 FROM made_operation RETURNING operation_id\
                 ) INSERT INTO native_objects(id,environment_id,operation_id,tenant_id,api_key_id,member_id,protocol,kind,model_id,provider_key_id,boundary,raw_id,parent_id,pending) \
                 SELECT $10,$1,operation_id,$11,$12,$13,'openai',$14,$15,$16,$17,$18,$9,false FROM made_source RETURNING *",
            )
            .bind(state.env.env_id).bind(operation_id).bind(op.dp_id).bind(op.tenant_id)
            .bind(op.api_key_id).bind(op.member_id).bind(op.visibility_full)
            .bind(serde_json::to_value(&derived).map_err(unavailable)?).bind(row.id)
            .bind(Uuid::new_v4()).bind(owner_tenant).bind(owner_key).bind(owner_member)
            .bind(object.kind.as_str()).bind(row.model_id).bind(row.provider_key_id)
            .bind(&row.boundary).bind(&object.raw_id)
            .fetch_one(&mut *conn).await.map_err(unavailable)?
        } else {
            sqlx::query("INSERT INTO native_operations(environment_id,operation_id,dp_id,tenant_id,api_key_id,member_id,visibility_full,submission,state) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'bound')")
                .bind(state.env.env_id).bind(operation_id).bind(op.dp_id).bind(op.tenant_id)
                .bind(op.api_key_id).bind(op.member_id).bind(op.visibility_full).bind(serde_json::to_value(&derived).map_err(unavailable)?)
                .execute(&mut *conn).await.map_err(unavailable)?;
            sqlx::query("INSERT INTO native_objects(id,environment_id,operation_id,tenant_id,api_key_id,member_id,protocol,kind,model_id,provider_key_id,boundary,raw_id,parent_id,pending) VALUES ($1,$2,$3,$4,$5,$6,'openai',$7,$8,$9,$10,$11,$12,false)")
                .bind(Uuid::new_v4()).bind(state.env.env_id).bind(operation_id).bind(owner_tenant)
                .bind(owner_key).bind(owner_member).bind(object.kind.as_str()).bind(row.model_id).bind(row.provider_key_id)
                .bind(&row.boundary).bind(&object.raw_id).bind(row.id)
                .execute(&mut *conn).await.map_err(unavailable)?;
            sqlx::query_as(
                "SELECT * FROM native_objects WHERE environment_id=$1 AND operation_id=$2",
            )
            .bind(state.env.env_id)
            .bind(operation_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(unavailable)?
        };
        insert_membership(
            &mut *conn,
            state,
            &row,
            &child,
            NativeRelation::ResponseOutput,
            body.operation_id,
        )
        .await?;
        if let Some(parent) = item_owner.filter(|_| conversation_writable) {
            bind_response_conversation_item(
                &mut *conn,
                state,
                parent,
                &child,
                body.operation_id,
                false,
            )
            .await?;
        }
        children.push(child.view(&derived)?);
    }
    let mut reply = NativeReply::new("bound");
    reply.record = Some(row.view(&submitted)?);
    reply.records = children;
    Ok(reply)
}

async fn bind_response_conversation_item(
    conn: &mut PgConnection,
    state: &AppState,
    parent: &ObjectRow,
    child: &ObjectRow,
    operation_id: Uuid,
    replayed_output: bool,
) -> Result<(), ApiError> {
    // 同一已接受操作的重放不能推进 epoch，也不能重新附加后来已移除的成员。
    let protocol: NativeProtocol =
        serde_json::from_value(json!(parent.protocol)).map_err(unavailable)?;
    let parent_kind: NativeObjectKind =
        serde_json::from_value(json!(parent.kind)).map_err(unavailable)?;
    let child_kind: NativeObjectKind =
        serde_json::from_value(json!(child.kind)).map_err(unavailable)?;
    let permitted = NativeRelation::ConversationItem.permits(protocol, parent_kind, child_kind)
        && parent.tenant_id == child.tenant_id
        && parent.provider_key_id == child.provider_key_id
        && parent.boundary == child.boundary
        && parent.deleted_at.is_none()
        && child.deleted_at.is_none();
    let accepted: bool = sqlx::query_scalar(
        "WITH prior AS MATERIALIZED (\
         SELECT COALESCE(MAX(generation),0)+1 AS next_generation, COALESCE(bool_or(deleted_at IS NULL OR created_operation_id=$4 OR $5),false) AS known \
         FROM native_object_memberships WHERE environment_id=$1 AND parent_id=$2 AND child_id=$3 AND relation='conversation_item'\
         ), made_membership AS (\
         INSERT INTO native_object_memberships(id,environment_id,parent_id,child_id,relation,created_operation_id,generation) \
         SELECT $6,$1,$2,$3,'conversation_item',$4,next_generation FROM prior WHERE NOT known AND $7 \
         ON CONFLICT(environment_id,parent_id,child_id,relation) WHERE deleted_at IS NULL DO NOTHING RETURNING 1\
         ), bumped AS (\
         UPDATE native_objects SET membership_epoch=membership_epoch+1,version=version+1 \
         WHERE environment_id=$1 AND id=$2 AND EXISTS(SELECT 1 FROM made_membership) RETURNING 1\
         ) SELECT known OR EXISTS(SELECT 1 FROM bumped) FROM prior",
    )
    .bind(state.env.env_id).bind(parent.id).bind(child.id).bind(operation_id).bind(replayed_output)
    .bind(Uuid::new_v4()).bind(permitted)
    .fetch_one(&mut *conn).await.map_err(unavailable)?;
    if !accepted {
        return Err(conflict());
    }
    Ok(())
}

async fn lookup_scoped(
    conn: &mut PgConnection,
    state: &AppState,
    body: LookupRequest,
    account_wide: bool,
) -> Result<NativeReply, ApiError> {
    if !text_ok(&body.locator.raw_id, 512) {
        return Err(invalid("/locator/raw_id"));
    }
    if body.consumer_family.is_some_and(|family| {
        !native_object_consumer_allowed(body.locator.protocol, body.locator.kind, family)
    }) {
        return Err(invalid("/consumer_family"));
    }
    let principal = principal(&mut *conn, body.api_key_id).await?;
    let boundary = body.locator.origin.as_ref().map(|o| o.boundary.as_str());
    let model = body.locator.origin.as_ref().map(|o| o.model_id);
    let provider = body.locator.origin.as_ref().map(|o| o.provider_key_id);
    let rows: Vec<ObjectRow> = sqlx::query_as(
        "SELECT * FROM native_objects WHERE environment_id = $1 AND tenant_id = $2 \
         AND protocol = $3 AND kind = $4 AND raw_id = $5 AND deleted_at IS NULL \
         AND ($6 OR (CASE WHEN $7::uuid IS NULL THEN api_key_id = $8 ELSE member_id = $7 END)) \
         AND ($9::text IS NULL OR (boundary = $9 AND ($13 OR model_id = $10) AND provider_key_id = $11)) \
         AND ($12::uuid IS NULL OR parent_id = $12) ORDER BY id LIMIT 2",
    )
    .bind(state.env.env_id)
    .bind(principal.tenant.id)
    .bind(body.locator.protocol.as_str())
    .bind(body.locator.kind.as_str())
    .bind(body.locator.raw_id)
    .bind(principal.full)
    .bind(principal.key.member_id)
    .bind(principal.key.id)
    .bind(boundary)
    .bind(model)
    .bind(provider)
    .bind(body.locator.parent_id)
    .bind(account_wide)
    .fetch_all(&mut *conn)
    .await
    .map_err(unavailable)?;
    if rows.len() > 1 {
        return Err(ApiError::conflict(
            "native_object_ambiguous",
            "请指定原生对象所属模型",
        ));
    }
    let row = rows.into_iter().next().ok_or_else(ApiError::not_found)?;
    authorized_for_consumer(&mut *conn, state, &principal, &row, body.consumer_family).await?;
    let submitted = submission(&operation(&mut *conn, state, row.operation_id, false).await?)?;
    if submitted.protocol == NativeProtocol::Openai
        && submitted.kind == NativeObjectKind::File
        && submitted.operation == "batch_input"
    {
        return Err(ApiError::not_found());
    }
    let mut reply = NativeReply::new("found");
    let mut view = row.view(&submitted)?;
    foundation::enrich_record(&mut *conn, state, &mut view).await?;
    reply.record = Some(view);
    Ok(reply)
}

async fn lookup(
    conn: &mut PgConnection,
    state: &AppState,
    body: LookupRequest,
) -> Result<NativeReply, ApiError> {
    let account_wide = body.consumer_family.is_some();
    lookup_scoped(conn, state, body, account_wide).await
}

async fn owned(
    conn: &mut PgConnection,
    state: &AppState,
    body: OwnedRequest,
) -> Result<NativeReply, ApiError> {
    if body.raw_ids.len() > MAX_PAGE || body.raw_ids.iter().any(|id| !text_ok(id, 512)) {
        return Err(invalid("/raw_ids"));
    }
    let mut reply = NativeReply::new("owned");
    for raw_id in body.raw_ids {
        let result = lookup_scoped(
            &mut *conn,
            state,
            LookupRequest {
                consumer_family: None,
                api_key_id: body.api_key_id,
                locator: ObjectLocator {
                    protocol: body.protocol,
                    kind: body.kind,
                    raw_id,
                    origin: Some(body.origin.clone()),
                    parent_id: body.parent_id,
                },
            },
            true,
        )
        .await;
        match result {
            Ok(found) => {
                if let Some(record) = found.record {
                    reply.records.push(record);
                }
            }
            Err(error) if error.status == axum::http::StatusCode::NOT_FOUND => {}
            Err(error) => return Err(error),
        }
    }
    // 空页仍检查当前密钥，不能把已撤销的身份当作合法空结果。
    principal(conn, body.api_key_id).await?;
    Ok(reply)
}

async fn tombstone(
    conn: &mut PgConnection,
    state: &AppState,
    body: TombstoneRequest,
) -> Result<NativeReply, ApiError> {
    let principal = principal(&mut *conn, body.api_key_id).await?;
    let row = object(&mut *conn, state, body.record_id).await?;
    authorized(&mut *conn, state, &principal, &row).await?;
    if row.version != body.version {
        return Err(conflict());
    }
    foundation::assert_unfenced(&mut *conn, state, row.id, None).await?;
    if row.kind == "message_batch" && row.completed_at.is_none() {
        // 删除不能让尚未结算的批次失去后续观察依据。
        return Err(conflict());
    }
    sqlx::query("UPDATE native_objects SET deleted_at = clock_timestamp(), version = version + 1 WHERE id = $1")
        .bind(row.id).execute(conn).await.map_err(unavailable)?;
    Ok(NativeReply::new("deleted"))
}

async fn claim(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: ClaimRequest,
) -> Result<NativeReply, ApiError> {
    if body.limit == 0 || body.limit > 50 {
        return Err(invalid("/limit"));
    }
    let rows: Vec<ObjectRow> = sqlx::query_as(
        "SELECT * FROM native_objects WHERE environment_id = $1 AND pending \
         AND poll_after <= clock_timestamp() AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp()) \
         AND ($2::uuid IS NULL OR id = $2) AND ($4::text IS NULL OR protocol = $4) \
         AND ($5::text IS NULL OR kind = $5) ORDER BY poll_after, id LIMIT $3 FOR UPDATE SKIP LOCKED",
    ).bind(state.env.env_id).bind(body.record_id).bind(i64::from(body.limit))
        .bind(body.protocol.map(|protocol| protocol.as_str()))
        .bind(body.kind.map(|kind| kind.as_str()))
        .fetch_all(&mut *conn).await.map_err(unavailable)?;
    let mut reply = NativeReply::new("leased");
    // 领取页同时按字节限制，避免多个大批次超过网关的有界回复读取上限。
    let mut reply_bytes = 0usize;
    for row in rows {
        let op = operation(&mut *conn, state, row.operation_id, false).await?;
        let submitted = submission(&op)?;
        // 不能用新凭据轮询旧账号对象；失败保留持久待处理项。
        if let Err(error) = origin(&mut *conn, state, None, &submitted).await {
            if error.code != "native_origin_changed" && error.code != "not_found" {
                return Err(error);
            }
            sqlx::query("UPDATE native_objects SET poll_after = clock_timestamp() + interval '1 minute' WHERE id = $1")
                .bind(row.id).execute(&mut *conn).await.map_err(unavailable)?;
            continue;
        }
        let view_bytes = serde_json::to_vec(&row.view(&submitted)?)
            .map_err(unavailable)?
            .len();
        if view_bytes > (7 * 1024 * 1024usize).saturating_sub(reply_bytes) {
            break;
        }
        reply_bytes += view_bytes;
        let row: ObjectRow = sqlx::query_as(
            "UPDATE native_objects SET lease_dp_id = $2, lease_expires_at = clock_timestamp() + make_interval(secs => $3), \
             lease_generation = lease_generation + 1 WHERE id = $1 RETURNING *",
        ).bind(row.id).bind(dp.dp_id).bind(LEASE_SECONDS as f64)
            .fetch_one(&mut *conn).await.map_err(unavailable)?;
        let mut view = row.view(&submitted)?;
        foundation::enrich_record(&mut *conn, state, &mut view).await?;
        if let Some(parent) = row.parent_id {
            view.parent_raw_id = sqlx::query_scalar(
                "SELECT raw_id FROM native_objects WHERE environment_id=$1 AND id=$2",
            )
            .bind(state.env.env_id)
            .bind(parent)
            .fetch_optional(&mut *conn)
            .await
            .map_err(unavailable)?;
        }
        reply.records.push(view);
    }
    Ok(reply)
}

async fn lease_live(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    token: &LeaseToken,
) -> Result<ObjectRow, ApiError> {
    let row = object(&mut *conn, state, token.record_id).await?;
    if row.lease_dp_id != Some(dp.dp_id) || row.lease_generation != token.generation {
        return Err(conflict());
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await
        .map_err(unavailable)?;
    if row.completed_at.is_none() && row.lease_expires_at.is_none_or(|at| at <= now) {
        return Err(conflict());
    }
    Ok(row)
}

async fn lease(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: LeaseRequest,
    renew: bool,
) -> Result<NativeReply, ApiError> {
    let row = lease_live(&mut *conn, state, dp, &body.lease).await?;
    if row.completed_at.is_some() {
        return Ok(NativeReply::new("completed"));
    }
    if body.poll_after_ms > 3_600_000 {
        return Err(invalid("/poll_after_ms"));
    }
    let row: ObjectRow = if renew {
        sqlx::query_as("UPDATE native_objects SET lease_expires_at = clock_timestamp() + make_interval(secs => $2) WHERE id = $1 RETURNING *")
            .bind(row.id).bind(LEASE_SECONDS as f64).fetch_one(&mut *conn).await.map_err(unavailable)?
    } else {
        sqlx::query_as("UPDATE native_objects SET poll_after = clock_timestamp() + make_interval(secs => $2), lease_expires_at = NULL WHERE id = $1 RETURNING *")
            .bind(row.id).bind(f64::from(body.poll_after_ms) / 1000.0).fetch_one(&mut *conn).await.map_err(unavailable)?
    };
    let submitted = submission(&operation(&mut *conn, state, row.operation_id, false).await?)?;
    let mut reply = NativeReply::new(if renew { "leased" } else { "pending" });
    reply.record = Some(row.view(&submitted)?);
    Ok(reply)
}

async fn complete(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: CompleteRequest,
) -> Result<NativeReply, ApiError> {
    let row = lease_live(&mut *conn, state, dp, &body.lease).await?;
    if row.completed_at.is_some() {
        return Ok(NativeReply::new("completed"));
    }
    let op = operation(&mut *conn, state, row.operation_id, false).await?;
    let submitted = submission(&op)?;
    let statuses: &[&str] = match (submitted.protocol, submitted.kind) {
        (NativeProtocol::Openai, NativeObjectKind::EvalRun) => &["completed", "failed", "canceled"],
        (NativeProtocol::Openai, _) => {
            &["completed", "failed", "cancelled", "incomplete", "expired"]
        }
        (NativeProtocol::Anthropic, _) => &["ended"],
    };
    if !statuses.contains(&body.terminal_status.as_str()) {
        return Err(invalid("/terminal_status"));
    }
    let expected: BTreeSet<&str> = if submitted.entries.is_empty() {
        BTreeSet::from([""])
    } else {
        submitted
            .entries
            .iter()
            .map(|entry| entry.entry_id.as_str())
            .collect()
    };
    let observed: BTreeSet<&str> = body
        .usage
        .iter()
        .map(|entry| entry.entry_id.as_str())
        .collect();
    let coverage_valid = if submitted.kind == NativeObjectKind::EvalRun {
        !observed.is_empty() && observed.is_subset(&expected)
    } else {
        observed == expected
    };
    if !coverage_valid || observed.len() != body.usage.len() {
        return Err(invalid("/usage"));
    }
    if body.output_file_ids.len() > 2
        || (!body.output_file_ids.is_empty()
            && (submitted.protocol != NativeProtocol::Openai
                || submitted.kind != NativeObjectKind::MessageBatch))
    {
        return Err(invalid("/output_file_ids"));
    }
    let output_ids: BTreeSet<_> = body.output_file_ids.iter().collect();
    if output_ids.len() != body.output_file_ids.len()
        || output_ids.iter().any(|id| {
            !text_ok(id, 256)
                || matches!(id.as_str(), "." | "..")
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        })
    {
        return Err(invalid("/output_file_ids"));
    }
    // 文件归属由有效租约从原提交派生；撤销密钥不能抹去已发生的推理及结果。
    for raw_id in output_ids {
        let mut derived = submitted.clone();
        let operation_id = Uuid::new_v4();
        derived.operation_id = operation_id;
        derived.kind = NativeObjectKind::File;
        derived.input_file_id = None;
        derived.parent_id = Some(row.id);
        derived.asynchronous = false;
        derived.operation = "batch_result".into();
        let existing: Option<ObjectRow> = sqlx::query_as("SELECT * FROM native_objects WHERE environment_id=$1 AND protocol='openai' AND kind='file' AND boundary=$2 AND raw_id=$3 FOR UPDATE")
            .bind(state.env.env_id).bind(&row.boundary).bind(raw_id)
            .fetch_optional(&mut *conn).await.map_err(unavailable)?;
        if let Some(existing) = existing {
            if existing.tenant_id != row.tenant_id
                || existing.api_key_id != row.api_key_id
                || existing.parent_id != Some(row.id)
                || existing.deleted_at.is_some()
            {
                return Err(conflict());
            }
            continue;
        }
        sqlx::query("INSERT INTO native_operations(environment_id,operation_id,dp_id,tenant_id,api_key_id,member_id,visibility_full,submission,state) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'bound')")
            .bind(state.env.env_id).bind(operation_id).bind(op.dp_id).bind(op.tenant_id)
            .bind(op.api_key_id).bind(op.member_id).bind(op.visibility_full).bind(serde_json::to_value(&derived).map_err(unavailable)?)
            .execute(&mut *conn).await.map_err(unavailable)?;
        sqlx::query("INSERT INTO native_objects(id,environment_id,operation_id,tenant_id,api_key_id,member_id,protocol,kind,model_id,provider_key_id,boundary,raw_id,parent_id,pending) VALUES ($1,$2,$3,$4,$5,$6,'openai','file',$7,$8,$9,$10,$11,false)")
            .bind(Uuid::new_v4()).bind(state.env.env_id).bind(operation_id).bind(op.tenant_id)
            .bind(op.api_key_id).bind(op.member_id).bind(row.model_id).bind(row.provider_key_id)
            .bind(&row.boundary).bind(raw_id).bind(row.id)
            .execute(&mut *conn).await.map_err(unavailable)?;
    }
    for usage in body.usage {
        if usage.cached_prompt_tokens > usage.prompt_tokens
            || usage
                .io_audit
                .as_ref()
                .is_some_and(|audit| audit.version != 2 || !audit.validate())
            || !(100..=599).contains(&usage.status_code)
            || usage.terminal_status.as_deref().is_some_and(|status| {
                if submitted.kind == NativeObjectKind::EvalRun {
                    !matches!(status, "completed" | "failed" | "canceled")
                } else {
                    !matches!(
                        status,
                        "completed" | "failed" | "cancelled" | "expired" | "incomplete"
                    )
                }
            })
            || usage.error_class.len() > 64
            || usage.error_message.len() > 4096
        {
            return Err(invalid("/usage"));
        }
        let entry = submitted
            .entries
            .iter()
            .find(|entry| entry.entry_id == usage.entry_id);
        let model_id = entry.map_or(submitted.origin.model_id, |entry| entry.model_id);
        let requested_id = entry.map_or(submitted.requested_model_id, |entry| {
            entry.requested_model_id
        });
        let event_id = Uuid::new_v4();
        let mut event = serde_json::to_value(&usage).map_err(unavailable)?;
        let fields = event.as_object_mut().ok_or_else(|| invalid("/usage"))?;
        fields.remove("entry_id");
        fields.remove("terminal_status");
        fields.insert("event_id".to_string(), json!(event_id));
        fields.insert("team_id".to_string(), json!(op.tenant_id));
        fields.insert("api_key_id".to_string(), json!(op.api_key_id));
        fields.insert(
            "user_id".to_string(),
            json!(op.member_id.map(|id| id.to_string()).unwrap_or_default()),
        );
        fields.insert("model_id".to_string(), json!(model_id));
        fields.insert("requested_model_id".to_string(), json!(requested_id));
        // 恢复任务只能观察实际用量，不能替换原请求已持久化的计价档位。
        fields.insert(
            "requested_service_tier".to_string(),
            json!(
                entry.map_or(submitted.requested_service_tier.as_ref(), |entry| entry
                    .requested_service_tier
                    .as_ref())
            ),
        );
        fields.insert(
            "is_batch".to_string(),
            json!(submitted.kind == NativeObjectKind::MessageBatch),
        );
        fields.insert(
            "provider_key_id".to_string(),
            json!(submitted.origin.provider_key_id),
        );
        fields.insert(
            "upstream_model".to_string(),
            json!(
                entry.map_or(submitted.upstream_model.as_str(), |entry| entry
                    .upstream_model
                    .as_str())
            ),
        );
        fields.insert(
            "upstream_provider".to_string(),
            json!(
                entry.map_or(submitted.upstream_provider.as_str(), |entry| entry
                    .upstream_provider
                    .as_str())
            ),
        );
        fields.insert(
            "requested_model".to_string(),
            json!(
                entry.map_or(submitted.requested_model.as_str(), |entry| entry
                    .requested_model
                    .as_str())
            ),
        );
        fields.insert("request_id".to_string(), json!(submitted.request_id));
        fields.insert("operation".to_string(), json!(submitted.operation));
        fields.insert("is_final_attempt".to_string(), json!(true));
        if matches!(
            submitted.kind,
            NativeObjectKind::Response | NativeObjectKind::MessageBatch | NativeObjectKind::EvalRun
        ) {
            // 唯一终态由对象锁和永久 outbox 身份保证；不按调用方 request_id 合并。
            fields.insert("upstream_dispatched".to_string(), json!(true));
            fields.insert(
                "inference_stage".to_string(),
                json!(InferenceStage::Terminal),
            );
            fields.insert("native_operation_id".to_string(), json!(row.operation_id));
            fields.insert(
                "native_terminal_status".to_string(),
                json!(usage
                    .terminal_status
                    .as_deref()
                    .unwrap_or(body.terminal_status.as_str())),
            );
        }
        sqlx::query("INSERT INTO native_terminal_outbox (event_id, object_id, entry_id, dp_id, event) VALUES ($1,$2,$3,$4,$5)")
            .bind(event_id).bind(row.id).bind(&usage.entry_id).bind(op.dp_id).bind(event)
            .execute(&mut *conn).await.map_err(unavailable)?;
    }
    sqlx::query("UPDATE native_objects SET pending = false, completed_at = clock_timestamp(), terminal_status = $2, version = version + 1 WHERE id = $1")
        .bind(row.id).bind(body.terminal_status).execute(conn).await.map_err(unavailable)?;
    Ok(NativeReply::new("completed"))
}

pub(crate) async fn execute(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    command: Command,
) -> Result<NativeReply, ApiError> {
    sqlx::query("SET LOCAL statement_timeout = '2s'")
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    // 同一提交可跨普通、引用、管理及实时入口重放，必须先于任何行锁串行。
    let operation_id = match &command {
        Command::Begin(body) => Some(body.operation_id),
        Command::Bind(body) => Some(body.operation_id),
        Command::BeginRelated(body) => Some(body.submission.operation_id),
        Command::BindRelated(body) => Some(body.binding.operation_id),
        Command::ReserveRelations(body) => Some(body.operation_id),
        Command::CommitRelations(body) => Some(body.operation_id),
        Command::ResolveReservation(body) => Some(body.operation_id),
        Command::ReserveDelete(body) => Some(body.operation_id),
        Command::CommitDelete(body) => Some(body.operation_id),
        Command::SignedSipIntake(body) => Some(body.operation_id),
        Command::MintCredential(body) => Some(body.operation_id),
        Command::CreateLiveSession(body) => Some(body.operation_id),
        Command::AppendLiveUsage(body) => Some(body.operation_id),
        Command::CheckpointLiveSession(body) => Some(body.lease.operation_id),
        Command::CompleteTyped(TypedCompleteRequest { target, .. })
        | Command::Reconciliation(ReconciliationRequest { target, .. }) => match target {
            MeteringTarget::Operation { operation_id, .. } => Some(*operation_id),
            MeteringTarget::Object { .. } => None,
        },
        Command::Lookup(_)
        | Command::Owned(_)
        | Command::Tombstone(_)
        | Command::Claim(_)
        | Command::Renew(_)
        | Command::Reschedule(_)
        | Command::Complete(_)
        | Command::LookupRelations(_)
        | Command::BindObservedArtifacts(_)
        | Command::RedeemCredential(_)
        | Command::ClaimLiveSessions(_) => None,
    };
    if let Some(operation_id) = operation_id {
        let identity = format!("native-operation:{}:{operation_id}", state.env.env_id);
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(identity)
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
    }
    match command {
        Command::Begin(body) => begin(conn, state, dp, *body).await,
        Command::Bind(body) => bind(conn, state, dp, body).await,
        Command::Lookup(body) => lookup(conn, state, body).await,
        Command::Owned(body) => owned(conn, state, body).await,
        Command::Tombstone(body) => tombstone(conn, state, body).await,
        Command::Claim(body) => claim(conn, state, dp, body).await,
        Command::Renew(body) => lease(conn, state, dp, body, true).await,
        Command::Reschedule(body) => lease(conn, state, dp, body, false).await,
        Command::Complete(body) => complete(conn, state, dp, body).await,
        Command::BeginRelated(body) => foundation::begin_related(conn, state, dp, body).await,
        Command::BindRelated(body) => foundation::bind_related(conn, state, dp, body).await,
        Command::ReserveRelations(body) => relations::reserve(conn, state, dp, body).await,
        Command::CommitRelations(body) => relations::commit(conn, state, dp, body).await,
        Command::LookupRelations(body) => relations::lookup(conn, state, dp, body).await,
        Command::ResolveReservation(body) => foundation::resolve(conn, state, dp, body).await,
        Command::BindObservedArtifacts(body) => foundation::observed(conn, state, dp, body).await,
        Command::ReserveDelete(body) => deletion::reserve(conn, state, dp, body).await,
        Command::CommitDelete(body) => deletion::commit(conn, state, dp, body).await,
        Command::SignedSipIntake(body) => realtime::sip_intake(conn, state, dp, body).await,
        Command::MintCredential(body) => realtime::mint(conn, state, dp, body).await,
        Command::RedeemCredential(body) => realtime::redeem(conn, state, dp, body).await,
        Command::CreateLiveSession(body) => realtime::create(conn, state, dp, body).await,
        Command::AppendLiveUsage(body) => realtime::append(conn, state, dp, body).await,
        Command::CheckpointLiveSession(body) => realtime::checkpoint(conn, state, dp, body).await,
        Command::ClaimLiveSessions(body) => realtime::claim(conn, state, dp, body).await,
        Command::CompleteTyped(body) => metering::complete_typed(conn, state, dp, body).await,
        Command::Reconciliation(body) => metering::mark_reconciliation(conn, state, dp, body).await,
    }
}

pub(crate) async fn deliver_one(
    conn: &mut PgConnection,
    state: &AppState,
) -> Result<bool, DbError> {
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *conn)
        .await?;
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT event_id, dp_id FROM native_terminal_outbox WHERE delivered_at IS NULL \
         AND dp_id = (SELECT dp_id FROM native_terminal_outbox WHERE delivered_at IS NULL \
                      ORDER BY created_at, event_id LIMIT 1) \
         ORDER BY created_at, event_id LIMIT 100 FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(&mut *conn)
    .await?;
    let Some((batch_id, dp_id)) = rows.first() else {
        return Ok(false);
    };
    let (batch_id, dp_id) = (*batch_id, *dp_id);
    let ids: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
    // 发件箱只存有界账务事实；同一查询从不可变提交恢复公共 caller 和精确条目审计。
    // 递推只计算字节前缀及首个超额事件，首事件始终可单独交付，避免大事件饿死。
    let rows: Vec<(Uuid, Uuid, Value)> = sqlx::query_as(
        r#"WITH RECURSIVE pending AS MATERIALIZED (
             SELECT outbox.event_id, outbox.entry_id, outbox.event, outbox.object_id,
                    row_number() OVER (ORDER BY outbox.created_at, outbox.event_id) AS ordinal
             FROM native_terminal_outbox outbox WHERE outbox.event_id = ANY($1)
           ), projected AS NOT MATERIALIZED (
             SELECT pending.ordinal, pending.event_id, objects.operation_id,
                    pending.event || COALESCE(observations.fields, '{}'::jsonb) AS event
             FROM pending
             JOIN native_objects objects ON objects.id = pending.object_id
             JOIN native_operations operations ON operations.environment_id = objects.environment_id
                                              AND operations.operation_id = objects.operation_id
             CROSS JOIN LATERAL (
               SELECT jsonb_path_query_first(operations.submission,
                        '$.entries[*] ? (@.entry_id == $entry_id)',
                        jsonb_build_object('entry_id', pending.entry_id)) AS entry
             ) matched
             CROSS JOIN LATERAL (
               SELECT operations.submission->'usage_context' AS caller,
                      CASE WHEN matched.entry IS NOT NULL THEN matched.entry->'usage_context'
                           WHEN pending.entry_id = '' THEN operations.submission->'usage_context'
                           ELSE NULL END AS audit
             ) context
             CROSS JOIN LATERAL (
               SELECT jsonb_object_agg(key, value) AS fields
               FROM jsonb_each(jsonb_build_object(
                 'api_key_name', context.caller->'api_key_name',
                 'tags', context.caller->'tags',
                 'jwt_subject', context.caller->'jwt_subject',
                 'jwt_provider', context.caller->'jwt_provider',
                 'jwt_claim_mapping', context.caller->'jwt_claim_mapping',
                 'applied_guardrails', context.audit->'applied_guardrails',
                 'guardrail_bypassed_reason', context.audit->'guardrail_bypassed_reason',
                 'guardrail_monitor_hits', context.audit->'guardrail_monitor_hits',
                 'guardrail_enforced_hits', context.audit->'guardrail_enforced_hits',
                 'guardrail_scores', context.audit->'guardrail_scores'))
               WHERE value NOT IN ('null'::jsonb, '""'::jsonb, '{}'::jsonb, '[]'::jsonb)
             ) observations
           ), bounded AS (
             SELECT projected.*, octet_length(projected.event::text)::bigint AS total_bytes
             FROM projected WHERE ordinal = 1
             UNION ALL
             SELECT next.*, bounded.total_bytes + octet_length(next.event::text)::bigint
             FROM bounded
             CROSS JOIN LATERAL (
               SELECT projected.* FROM projected
               WHERE projected.ordinal = bounded.ordinal + 1 AND bounded.total_bytes <= $2
               OFFSET 0
             ) next
           )
           SELECT event_id, operation_id, event FROM bounded
           WHERE total_bytes <= $2 OR ordinal = 1 ORDER BY ordinal"#,
    )
    .bind(&ids)
    .bind(8 * 1024 * 1024i64)
    .fetch_all(&mut *conn)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|(id, _, _)| *id).collect();
    let operation_ids: Vec<Uuid> = rows
        .iter()
        .map(|(_, operation_id, _)| *operation_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let events = rows.into_iter().map(|(_, _, event)| event).collect();
    // 同一提交网关的一批事件一次接收，沿用原有账号锁的全局顺序。
    let outcome =
        crate::usage::ingest_transaction(&mut *conn, &state.prices, dp_id, batch_id, events)
            .await?;
    if outcome.rejected != 0 {
        return Err(DbError::Db(sqlx::Error::Protocol(
            "native terminal event was rejected".to_string(),
        )));
    }
    sqlx::query("UPDATE native_terminal_outbox SET delivered_at = clock_timestamp() WHERE event_id = ANY($1)")
        .bind(ids).execute(&mut *conn).await?;
    metering::delivered(&mut *conn, state, &operation_ids, outcome.unpriced > 0).await?;
    Ok(true)
}

#[cfg(test)]
mod consumer_endpoint_tests {
    use super::*;
    #[test]
    fn wildcard_model_authority_keeps_the_concrete_accepted_name() {
        let accepted:BeginRequest=serde_json::from_value(json!({
            "operation_id":Uuid::from_u128(1),"api_key_id":Uuid::from_u128(2),
            "protocol":"openai","kind":"file",
            "origin":{"model_id":Uuid::from_u128(3),"provider_key_id":Uuid::from_u128(4),"boundary":"v1:0000000000000000000000000000000000000000000000000000000000000000"},
            "requested_model_id":Uuid::from_u128(3),"requested_model":"openai/gpt-4",
            "upstream_model":"gpt-4","operation":"files","request_id":"wildcard-contract","asynchronous":false
        })).unwrap();
        let mut model: Model = serde_json::from_value(
            json!({"display_name":"openai/*","provider":"openai","model_name":"gpt-4"}),
        )
        .unwrap();
        assert_eq!(
            requested_authority_name(&model, &accepted),
            Some("openai/gpt-4")
        );
        model.display_name = "anthropic/*".into();
        assert_eq!(requested_authority_name(&model, &accepted), None);
        model.display_name = "renamed-model".into();
        assert_eq!(
            requested_authority_name(&model, &accepted),
            Some("renamed-model")
        );
    }

    #[test]
    fn file_consumer_selects_only_its_actual_endpoint_without_granting_file_management() {
        let key: ApiKey =
            serde_json::from_value(json!({"key_hash":"fixture","allowed_endpoints":["batches"]}))
                .unwrap();
        assert!(endpoint_permitted(
            &key,
            NativeObjectKind::File,
            Some(EndpointFamily::Batches)
        ));
        assert!(!endpoint_permitted(&key, NativeObjectKind::File, None));
        assert!(!endpoint_permitted(
            &key,
            NativeObjectKind::File,
            Some(EndpointFamily::Responses)
        ));
    }
}

pub(crate) async fn deliver_live_one(
    conn: &mut PgConnection,
    state: &AppState,
) -> Result<bool, DbError> {
    realtime::deliver_one(conn, state).await
}
