//! 实时会话的授权、追加日志和投递事务；已受理用量不依赖原密钥继续有效。

use aisix_core::native_metering::{validate_observed_usage, ObservedMeter, ObservedUsage};
use aisix_core::native_realtime::*;
use base64::Engine as _;

use super::*;

#[derive(sqlx::FromRow)]
struct CredentialRow {
    id: Uuid,
    operation_id: Uuid,
    tenant_id: Uuid,
    api_key_id: Uuid,
    member_id: Option<Uuid>,
    creator_key_hash: String,
    purpose: String,
    expires_at: DateTime<Utc>,
    defaults: Value,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct LiveRow {
    operation_id: Uuid,
    api_key_id: Uuid,
    credential_id: Option<Uuid>,
    purpose: String,
    transport: String,
    provider_session_id: Option<String>,
    state: String,
    coverage: String,
    accounting_state: String,
    lease_dp_id: Uuid,
    lease_generation: i64,
    lease_expires_at: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct JournalRow {
    event_id: String,
    response_id: Option<String>,
    usage: Value,
    raw_usage: Option<Value>,
    coverage: String,
    terminal: bool,
}

fn purpose(value: RealtimePurpose) -> Result<&'static str, ApiError> {
    match value {
        RealtimePurpose::Realtime => Ok("realtime"),
        RealtimePurpose::Transcription => Ok("transcription"),
        RealtimePurpose::Translation => Ok("translation"),
        RealtimePurpose::Unknown => Err(invalid("/purpose")),
    }
}

fn transport(value: RealtimeTransport) -> Result<&'static str, ApiError> {
    match value {
        RealtimeTransport::WebSocket => Ok("web_socket"),
        RealtimeTransport::WebRtc => Ok("web_rtc"),
        RealtimeTransport::Sip => Ok("sip"),
        RealtimeTransport::Unknown => Err(invalid("/transport")),
    }
}

fn coverage(value: UsageCoverage) -> &'static str {
    match value {
        UsageCoverage::Partial => "partial",
        UsageCoverage::Complete => "complete",
        UsageCoverage::Unknown => "unknown",
    }
}

fn parse_purpose(value: &str) -> RealtimePurpose {
    match value {
        "realtime" => RealtimePurpose::Realtime,
        "transcription" => RealtimePurpose::Transcription,
        "translation" => RealtimePurpose::Translation,
        _ => RealtimePurpose::Unknown,
    }
}

fn parse_coverage(value: &str) -> UsageCoverage {
    match value {
        "complete" => UsageCoverage::Complete,
        "partial" => UsageCoverage::Partial,
        _ => UsageCoverage::Unknown,
    }
}

fn token_hash_valid(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// 持久元数据不能夹带供应商密钥槽，也不能夹带完整音频或事件正文。
fn metadata_safe(value: &Value, usage_only: bool) -> bool {
    if !usage_only {
        // 工具参数的 JSON Schema 可合法声明 value/token 等名字，不递归审查其字段名。
        let safe_slot = |value: &Value| {
            value.is_null()
                || value.as_object().is_some_and(|fields| {
                    !fields.keys().any(|key| {
                        matches!(
                            key.to_ascii_lowercase().as_str(),
                            "client_secret"
                                | "value"
                                | "api_key"
                                | "authorization"
                                | "access_token"
                                | "refresh_token"
                        )
                    })
                })
        };
        return safe_slot(value) && value.get("session").is_none_or(safe_slot);
    }
    match value {
        Value::Object(fields) => fields.iter().all(|(key, value)| {
            !matches!(
                key.to_ascii_lowercase().as_str(),
                "client_secret"
                    | "api_key"
                    | "access_token"
                    | "refresh_token"
                    | "authorization"
                    | "password"
                    | "secret"
                    | "token"
                    | "value"
            ) && (!usage_only
                || !matches!(
                    key.to_ascii_lowercase().as_str(),
                    "audio" | "text" | "transcript" | "delta" | "instructions" | "session" | "sdp"
                ))
                && metadata_safe(value, usage_only)
        }),
        Value::Array(entries) => entries.iter().all(|value| metadata_safe(value, usage_only)),
        _ => true,
    }
}

async fn now(conn: &mut PgConnection) -> Result<DateTime<Utc>, ApiError> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(conn)
        .await
        .map_err(unavailable)
}

async fn realtime_principal(conn: &mut PgConnection, id: Uuid) -> Result<Principal, ApiError> {
    let current = principal(conn, id).await?;
    let key: ApiKey =
        serde_json::from_value(keys::build_key_doc(&current.tenant, &current.key, true))
            .map_err(unavailable)?;
    if !key.can_call(EndpointFamily::Realtime) {
        return Err(ApiError::not_found());
    }
    Ok(current)
}

fn require_live_submission(submitted: &BeginRequest) -> Result<(), ApiError> {
    if submitted.protocol != NativeProtocol::Openai
        || submitted.kind != NativeObjectKind::RealtimeCall
    {
        return Err(invalid("/operation_id"));
    }
    Ok(())
}

async fn credential_by_hash(
    conn: &mut PgConnection,
    state: &AppState,
    hash: &str,
) -> Result<CredentialRow, ApiError> {
    sqlx::query_as("SELECT id,operation_id,tenant_id,api_key_id,member_id,creator_key_hash,purpose,expires_at,defaults,revoked_at FROM native_realtime_credentials WHERE environment_id=$1 AND token_hash=$2")
        .bind(state.env.env_id).bind(hash).fetch_optional(conn).await.map_err(unavailable)?
        .ok_or_else(ApiError::not_found)
}

async fn credential_by_id(
    conn: &mut PgConnection,
    state: &AppState,
    id: Uuid,
) -> Result<CredentialRow, ApiError> {
    sqlx::query_as("SELECT id,operation_id,tenant_id,api_key_id,member_id,creator_key_hash,purpose,expires_at,defaults,revoked_at FROM native_realtime_credentials WHERE environment_id=$1 AND id=$2")
        .bind(state.env.env_id).bind(id).fetch_optional(conn).await.map_err(unavailable)?
        .ok_or_else(ApiError::not_found)
}

fn credential_current(
    row: &CredentialRow,
    current: &Principal,
    desired: &str,
    at: DateTime<Utc>,
) -> Result<(), ApiError> {
    if row.tenant_id != current.tenant.id
        || row.api_key_id != current.key.id
        || row.member_id != current.key.member_id
        || row.creator_key_hash != current.key.key_hash
        || row.purpose != desired
        || row.expires_at <= at
        || row.revoked_at.is_some()
    {
        return Err(ApiError::not_found());
    }
    Ok(())
}

async fn credential_reply(
    conn: &mut PgConnection,
    state: &AppState,
    row: CredentialRow,
) -> Result<NativeReply, ApiError> {
    let op = operation(conn, state, row.operation_id, false).await?;
    let submitted = submission(&op)?;
    let mut reply = NativeReply::new("credential");
    reply.credential = Some(NativeCredential {
        credential_id: row.id,
        api_key_id: row.api_key_id,
        purpose: parse_purpose(&row.purpose),
        expires_at: Some(row.expires_at),
        defaults: row.defaults,
        origin: Some(submitted.origin),
        requested_model_id: submitted.requested_model_id,
        requested_model: submitted.requested_model,
    });
    Ok(reply)
}

pub(super) async fn mint(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: MintCredentialRequest,
) -> Result<NativeReply, ApiError> {
    let desired = purpose(body.purpose)?;
    if !token_hash_valid(&body.token_hash)
        || !metadata_safe(&body.defaults, false)
        || serde_json::to_vec(&body.defaults)
            .map_err(unavailable)?
            .len()
            > 256 * 1024
    {
        return Err(invalid("/credential"));
    }
    let current = realtime_principal(&mut *conn, body.api_key_id).await?;
    let at = now(&mut *conn).await?;
    if body.expires_at <= at || body.expires_at > at + chrono::Duration::hours(2) {
        return Err(invalid("/expires_at"));
    }
    let op = operation(&mut *conn, state, body.operation_id, true).await?;
    if op.dp_id != dp.dp_id || op.api_key_id != current.key.id || op.tenant_id != current.tenant.id
    {
        return Err(ApiError::not_found());
    }
    let submitted = submission(&op)?;
    require_live_submission(&submitted)?;
    origin_for_consumer(
        &mut *conn,
        state,
        Some(&current),
        &submitted,
        Some(EndpointFamily::Realtime),
    )
    .await?;
    let prior: Option<(String,)> = sqlx::query_as("SELECT token_hash FROM native_realtime_credentials WHERE environment_id=$1 AND operation_id=$2")
        .bind(state.env.env_id).bind(body.operation_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
    if let Some((hash,)) = prior {
        let row = credential_by_hash(&mut *conn, state, &hash).await?;
        credential_current(&row, &current, desired, at)?;
        if hash != body.token_hash
            || row.expires_at != body.expires_at
            || row.defaults != body.defaults
        {
            return Err(conflict());
        }
        return credential_reply(conn, state, row).await;
    }
    let role: Option<String> = match current.key.member_id {
        Some(member) => sqlx::query_scalar("SELECT role FROM members WHERE id=$1 AND tenant_id=$2")
            .bind(member)
            .bind(current.tenant.id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(unavailable)?,
        None => None,
    };
    sqlx::query("INSERT INTO native_realtime_credentials(id,environment_id,operation_id,token_hash,tenant_id,api_key_id,member_id,creator_key_hash,creator_key_version,creator_member_role,visibility_full,purpose,expires_at,defaults) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
        .bind(Uuid::new_v4()).bind(state.env.env_id).bind(body.operation_id).bind(&body.token_hash)
        .bind(current.tenant.id).bind(current.key.id).bind(current.key.member_id)
        .bind(&current.key.key_hash).bind(current.key.version).bind(role).bind(current.full)
        .bind(desired).bind(body.expires_at).bind(body.defaults).execute(&mut *conn).await.map_err(unavailable)?;
    let row = credential_by_hash(&mut *conn, state, &body.token_hash).await?;
    credential_reply(conn, state, row).await
}

pub(super) async fn redeem(
    conn: &mut PgConnection,
    state: &AppState,
    _dp: DpIdentity,
    body: RedeemCredentialRequest,
) -> Result<NativeReply, ApiError> {
    let desired = purpose(body.purpose)?;
    if !token_hash_valid(&body.token_hash)
        || body
            .client_ip
            .as_ref()
            .is_some_and(|ip| ip.parse::<std::net::IpAddr>().is_err())
    {
        return Err(ApiError::not_found());
    }
    let row = credential_by_hash(&mut *conn, state, &body.token_hash).await?;
    let current = realtime_principal(&mut *conn, row.api_key_id).await?;
    credential_current(&row, &current, desired, now(&mut *conn).await?)?;
    // 不复查发行时默认模型；最终配置由新 begin 和当前数据面模型路由共同授权。
    credential_reply(conn, state, row).await
}

async fn live(conn: &mut PgConnection, state: &AppState, id: Uuid) -> Result<LiveRow, ApiError> {
    sqlx::query_as("SELECT operation_id,api_key_id,credential_id,purpose,transport,provider_session_id,state,coverage,accounting_state,lease_dp_id,lease_generation,lease_expires_at,deadline_at FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2 FOR UPDATE")
        .bind(state.env.env_id).bind(id).fetch_optional(conn).await.map_err(unavailable)?
        .ok_or_else(ApiError::not_found)
}

fn live_record(row: &LiveRow) -> LiveSessionRecord {
    LiveSessionRecord {
        operation_id: row.operation_id,
        api_key_id: row.api_key_id,
        purpose: parse_purpose(&row.purpose),
        transport: match row.transport.as_str() {
            "web_socket" => RealtimeTransport::WebSocket,
            "web_rtc" => RealtimeTransport::WebRtc,
            "sip" => RealtimeTransport::Sip,
            _ => RealtimeTransport::Unknown,
        },
        provider_session_id: row.provider_session_id.clone(),
        lease: Some(LiveSessionLease {
            operation_id: row.operation_id,
            generation: row.lease_generation,
        }),
        lease_expires_at: Some(row.lease_expires_at),
        deadline_at: Some(row.deadline_at),
        state: row.state.clone(),
        coverage: parse_coverage(&row.coverage),
        accounting_state: row.accounting_state.clone(),
        requires_stop: row.state == "requires_stop",
        accepted: None,
    }
}

fn live_reply(row: &LiveRow, appended: Option<bool>) -> NativeReply {
    let mut reply = NativeReply::new(&row.state);
    reply.operation_id = Some(row.operation_id);
    reply.metering_state = Some(row.accounting_state.clone());
    reply.reconciliation_reason =
        (row.coverage == "unknown").then(|| "incomplete_observation".into());
    reply.live_session = Some(live_record(row));
    reply.appended = appended;
    reply
}

pub(super) async fn create(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: CreateLiveSessionRequest,
) -> Result<NativeReply, ApiError> {
    create_inner(conn, state, dp, body, false).await
}

async fn create_inner(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: CreateLiveSessionRequest,
    verified_sip: bool,
) -> Result<NativeReply, ApiError> {
    let desired = purpose(body.purpose)?;
    let transport = transport(body.transport)?;
    if body
        .provider_session_id
        .as_ref()
        .is_some_and(|id| !text_ok(id, 512))
    {
        return Err(invalid("/provider_session_id"));
    }
    // 入站 SIP 必须由专属签名路由的可信绑定派生，普通会话准入不能自报归属。
    if body.transport == RealtimeTransport::Sip && !verified_sip {
        return Err(invalid("/transport"));
    }
    let current = realtime_principal(&mut *conn, body.api_key_id).await?;
    let op = operation(&mut *conn, state, body.operation_id, true).await?;
    if op.dp_id != dp.dp_id || op.api_key_id != current.key.id || op.tenant_id != current.tenant.id
    {
        return Err(ApiError::not_found());
    }
    let submitted = submission(&op)?;
    require_live_submission(&submitted)?;
    origin_for_consumer(
        &mut *conn,
        state,
        Some(&current),
        &submitted,
        Some(EndpointFamily::Realtime),
    )
    .await?;
    let at = now(&mut *conn).await?;
    let previous: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2)")
        .bind(state.env.env_id).bind(body.operation_id).fetch_one(&mut *conn).await.map_err(unavailable)?;
    if previous {
        let row = live(&mut *conn, state, body.operation_id).await?;
        if row.api_key_id != body.api_key_id
            || row.purpose != desired
            || row.transport != transport
            || row.credential_id != body.credential_id
            || row.provider_session_id != body.provider_session_id
            || row.deadline_at != body.deadline_at
            || row.lease_dp_id != dp.dp_id
        {
            return Err(conflict());
        }
        return Ok(live_reply(&row, None));
    }
    if body.deadline_at <= at {
        return Err(invalid("/deadline_at"));
    }
    if let Some(id) = body.credential_id {
        let credential = credential_by_id(&mut *conn, state, id).await?;
        credential_current(&credential, &current, desired, at)?;
    }
    sqlx::query("INSERT INTO native_live_sessions(environment_id,operation_id,tenant_id,api_key_id,member_id,credential_id,purpose,transport,provider_session_id,state,coverage,accounting_state,lease_dp_id,lease_generation,lease_expires_at,deadline_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'live','partial','pending',$10,1,LEAST(clock_timestamp()+interval '30 seconds',$11),$11)")
        .bind(state.env.env_id).bind(body.operation_id).bind(op.tenant_id).bind(op.api_key_id).bind(op.member_id)
        .bind(body.credential_id).bind(desired).bind(transport).bind(body.provider_session_id).bind(dp.dp_id)
        .bind(body.deadline_at).execute(&mut *conn).await.map_err(unavailable)?;
    accept_entries(&mut *conn, state, &current, &submitted, &submitted.entries).await?;
    let row = live(conn, state, body.operation_id).await?;
    Ok(live_reply(&row, None))
}

fn lease_owner(
    row: &LiveRow,
    dp: DpIdentity,
    lease: Option<&LiveSessionLease>,
) -> Result<(), ApiError> {
    if row.lease_dp_id != dp.dp_id
        || lease.is_some_and(|lease| {
            lease.operation_id != row.operation_id || lease.generation != row.lease_generation
        })
        || (lease.is_none() && row.lease_generation != 1)
    {
        return Err(ApiError::not_found());
    }
    Ok(())
}

fn normalized_measurement(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(entries) = value.as_array_mut() {
        entries.sort_by(|left, right| {
            left.get("entry_id")
                .and_then(Value::as_str)
                .cmp(&right.get("entry_id").and_then(Value::as_str))
        });
        for entry in entries {
            if let Some(fields) = entry.as_object_mut() {
                fields.remove("occurred_at");
            }
            if let Some(fields) = entry
                .pointer_mut("/meter/usage")
                .and_then(Value::as_object_mut)
            {
                fields.remove("occurred_at");
            }
        }
    }
    value
}

fn validate_usage(usage: &[ObservedUsage]) -> Result<(), ApiError> {
    if usage.len() > MAX_ENTRIES || (!usage.is_empty() && validate_observed_usage(usage).is_err())
        || usage.iter().any(|entry| entry.entry_id.len()>256 || entry.entry_id.chars().any(char::is_control)
            || !text_ok(&entry.upstream_model,256)
            || matches!(&entry.meter, ObservedMeter::Tokens { usage } if usage.cached_prompt_tokens>usage.prompt_tokens
                || !(100..=599).contains(&usage.status_code) || usage.error_class.len()>64 || usage.error_message.len()>4096
                || usage.io_audit.as_ref().is_some_and(|audit| audit.version!=2 || !audit.validate()))) {
        return Err(invalid("/usage"));
    }
    Ok(())
}

fn usage_event(
    op: &OperationRow,
    submitted: &BeginRequest,
    observation: &ObservedUsage,
    operation_id: Uuid,
    event_id: Uuid,
) -> Result<Option<Value>, ApiError> {
    let ObservedMeter::Tokens { usage } = &observation.meter else {
        // 未接入既有价目契约的单位留在 journal 待对账，不能伪装成零 token。
        return Ok(None);
    };
    let entry = submitted
        .entries
        .iter()
        .find(|entry| entry.entry_id == observation.entry_id);
    if !observation.entry_id.is_empty() && entry.is_none() {
        return Ok(None);
    }
    let upstream_model = entry.map_or(submitted.upstream_model.as_str(), |entry| {
        entry.upstream_model.as_str()
    });
    if observation.upstream_model != upstream_model {
        // 未找到独立计费模型的已授权快照仍保留真实观察，不能错记主模型。
        return Ok(None);
    }
    let mut event = serde_json::to_value(usage).map_err(unavailable)?;
    let fields = event.as_object_mut().ok_or_else(|| invalid("/usage"))?;
    fields.remove("entry_id");
    fields.remove("terminal_status");
    fields.extend([
        ("event_id".into(), json!(event_id)),
        ("occurred_at".into(), json!(observation.occurred_at)),
        ("team_id".into(), json!(op.tenant_id)),
        ("api_key_id".into(), json!(op.api_key_id)),
        (
            "user_id".into(),
            json!(op.member_id.map(|id| id.to_string()).unwrap_or_default()),
        ),
        (
            "model_id".into(),
            json!(entry.map_or(submitted.origin.model_id, |entry| entry.model_id)),
        ),
        (
            "requested_model_id".into(),
            json!(entry.map_or(submitted.requested_model_id, |entry| entry
                .requested_model_id)),
        ),
        (
            "provider_key_id".into(),
            json!(submitted.origin.provider_key_id),
        ),
        ("upstream_model".into(), json!(upstream_model)),
        (
            "upstream_provider".into(),
            json!(
                entry.map_or(submitted.upstream_provider.as_str(), |entry| entry
                    .upstream_provider
                    .as_str())
            ),
        ),
        (
            "requested_model".into(),
            json!(
                entry.map_or(submitted.requested_model.as_str(), |entry| entry
                    .requested_model
                    .as_str())
            ),
        ),
        (
            "requested_service_tier".into(),
            json!(entry
                .and_then(|entry| entry.requested_service_tier.as_ref())
                .or(submitted.requested_service_tier.as_ref())),
        ),
        ("request_id".into(), json!(submitted.request_id)),
        (
            "operation".into(),
            json!(if observation.entry_id.is_empty() {
                submitted.operation.as_str()
            } else {
                "transcription"
            }),
        ),
        ("inbound_protocol".into(), json!("realtime")),
        ("is_final_attempt".into(), json!(true)),
        ("is_batch".into(), json!(false)),
        ("upstream_dispatched".into(), json!(true)),
        ("inference_stage".into(), json!(InferenceStage::Terminal)),
        ("native_operation_id".into(), json!(operation_id)),
    ]);
    Ok(Some(event))
}

pub(super) async fn append(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: AppendLiveUsageRequest,
) -> Result<NativeReply, ApiError> {
    if !text_ok(&body.event_id, 512)
        || body
            .response_id
            .as_ref()
            .is_some_and(|id| !text_ok(id, 512))
        || body
            .raw_usage
            .as_ref()
            .is_some_and(|usage| !metadata_safe(usage, true))
    {
        return Err(invalid("/event_id"));
    }
    validate_usage(&body.usage)?;
    let usage = serde_json::to_value(&body.usage).map_err(unavailable)?;
    if serde_json::to_vec(&usage).map_err(unavailable)?.len() > 1024 * 1024
        || body
            .raw_usage
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(unavailable)?
            .is_some_and(|value| value.len() > 256 * 1024)
    {
        return Err(invalid("/usage"));
    }
    let row = live(&mut *conn, state, body.operation_id).await?;
    if row.api_key_id != body.api_key_id {
        return Err(ApiError::not_found());
    }
    lease_owner(&row, dp, body.lease.as_ref())?;
    let old: Vec<JournalRow> = sqlx::query_as("SELECT event_id,response_id,usage,raw_usage,coverage,terminal FROM native_live_journal WHERE environment_id=$1 AND operation_id=$2 AND (event_id=$3 OR ($4::text IS NOT NULL AND response_id=$4))")
        .bind(state.env.env_id).bind(body.operation_id).bind(&body.event_id).bind(&body.response_id)
        .fetch_all(&mut *conn).await.map_err(unavailable)?;
    let event_coverage = if body.terminal && body.usage.is_empty() {
        "unknown"
    } else {
        coverage(body.coverage)
    };
    if !old.is_empty() {
        if old.iter().any(|old| {
            (old.event_id == body.event_id && old.response_id != body.response_id)
                || normalized_measurement(&old.usage) != normalized_measurement(&usage)
                || old.raw_usage != body.raw_usage
                || old.coverage != event_coverage
                || old.terminal != body.terminal
        }) {
            return Err(conflict());
        }
        return Ok(live_reply(&row, Some(false)));
    }
    // 已接受的尾量保留原 holder/generation；新工作和续租仍由 checkpoint/claim 裁决。
    let op = operation(&mut *conn, state, body.operation_id, false).await?;
    let mut submitted = submission(&op)?;
    require_live_submission(&submitted)?;
    let entries:Vec<Value>=sqlx::query_scalar("SELECT entry FROM native_live_entries WHERE environment_id=$1 AND operation_id=$2 ORDER BY entry_id")
        .bind(state.env.env_id).bind(body.operation_id).fetch_all(&mut *conn).await.map_err(unavailable)?;
    submitted.entries = entries
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(unavailable)?;
    let journal_id = Uuid::new_v4();
    sqlx::query("INSERT INTO native_live_journal(id,environment_id,operation_id,event_id,response_id,observer_dp_id,usage,raw_usage,coverage,terminal) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
        .bind(journal_id).bind(state.env.env_id).bind(body.operation_id).bind(&body.event_id).bind(&body.response_id)
        .bind(dp.dp_id).bind(usage).bind(body.raw_usage).bind(event_coverage).bind(body.terminal)
        .execute(&mut *conn).await.map_err(unavailable)?;
    let mut unpriced = false;
    for observation in &body.usage {
        let event_id = Uuid::new_v4();
        if let Some(event) = usage_event(&op, &submitted, observation, body.operation_id, event_id)?
        {
            sqlx::query("INSERT INTO native_live_outbox(event_id,journal_id,entry_id,dp_id,event) VALUES ($1,$2,$3,$4,$5)")
                .bind(event_id).bind(journal_id).bind(&observation.entry_id).bind(op.dp_id).bind(event)
                .execute(&mut *conn).await.map_err(unavailable)?;
        } else {
            unpriced = true;
        }
    }
    let lost =
        row.coverage == "unknown" || event_coverage == "unknown" || row.state == "requires_stop";
    let next_coverage = if lost { "unknown" } else { event_coverage };
    let accounting = if lost || unpriced || row.accounting_state == "awaiting_reconciliation" {
        "awaiting_reconciliation"
    } else if body.usage.is_empty() {
        row.accounting_state.as_str()
    } else {
        "observed"
    };
    sqlx::query("UPDATE native_live_sessions SET state=CASE WHEN $3 THEN 'closed' ELSE state END,closed_at=CASE WHEN $3 THEN COALESCE(closed_at,clock_timestamp()) ELSE closed_at END,coverage=$4,accounting_state=$5,updated_at=clock_timestamp() WHERE environment_id=$1 AND operation_id=$2")
        .bind(state.env.env_id).bind(body.operation_id).bind(body.terminal).bind(next_coverage).bind(accounting)
        .execute(&mut *conn).await.map_err(unavailable)?;
    refresh_accounting(&mut *conn, state, body.operation_id)
        .await
        .map_err(unavailable)?;
    let row = live(conn, state, body.operation_id).await?;
    Ok(live_reply(&row, Some(true)))
}

pub(super) async fn checkpoint(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: CheckpointLiveSessionRequest,
) -> Result<NativeReply, ApiError> {
    let row = live(&mut *conn, state, body.lease.operation_id).await?;
    lease_owner(&row, dp, Some(&body.lease))?;
    let at = now(&mut *conn).await?;
    if row.state == "closed" {
        return Ok(live_reply(&row, None));
    }
    if row.lease_expires_at <= at {
        return Err(ApiError::not_found());
    }
    let op = operation(&mut *conn, state, row.operation_id, false).await?;
    let submitted = submission(&op)?;
    let mut authorization_lost = false;
    if !body.terminal {
        let authorized = match realtime_principal(&mut *conn, row.api_key_id).await {
            Ok(current) => async {
                origin_for_consumer(
                    &mut *conn,
                    state,
                    Some(&current),
                    &submitted,
                    Some(EndpointFamily::Realtime),
                )
                .await?;
                let entries: Vec<Value> = sqlx::query_scalar("SELECT entry FROM native_live_entries WHERE environment_id=$1 AND operation_id=$2 ORDER BY entry_id")
                    .bind(state.env.env_id).bind(row.operation_id).fetch_all(&mut *conn).await.map_err(unavailable)?;
                for encoded in entries {
                    let entry: NativeEntry = serde_json::from_value(encoded).map_err(unavailable)?;
                    if !entry_model_current(&mut *conn, state, &current, &submitted, &entry).await? {
                        return Err(conflict());
                    }
                }
                Ok(())
            }
            .await,
            Err(error) => Err(error),
        };
        if let Err(error) = authorized {
            if matches!(
                error.status,
                axum::http::StatusCode::NOT_FOUND | axum::http::StatusCode::CONFLICT
            ) {
                authorization_lost = true;
            } else {
                return Err(error);
            }
        }
    }
    if !body.entries.is_empty() && !authorization_lost {
        if row.state != "live" || row.deadline_at <= at || body.terminal {
            return Err(conflict());
        }
        let current = realtime_principal(&mut *conn, row.api_key_id).await?;
        accept_entries(&mut *conn, state, &current, &submitted, &body.entries).await?;
    }
    let has_usage:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_live_journal WHERE environment_id=$1 AND operation_id=$2 AND jsonb_array_length(usage)>0)")
        .bind(state.env.env_id).bind(row.operation_id).fetch_one(&mut *conn).await.map_err(unavailable)?;
    let lost = row.coverage == "unknown"
        || body.coverage == UsageCoverage::Unknown
        || row.deadline_at <= at
        || row.state == "requires_stop"
        || authorization_lost
        || (body.terminal && !has_usage);
    let next = if lost {
        "unknown"
    } else {
        coverage(body.coverage)
    };
    let next_state = if body.terminal {
        "closed"
    } else if row.deadline_at <= at || authorization_lost {
        "requires_stop"
    } else {
        row.state.as_str()
    };
    sqlx::query("UPDATE native_live_sessions SET state=$3,coverage=$4,accounting_state=CASE WHEN $4='unknown' THEN 'awaiting_reconciliation' ELSE accounting_state END,lease_expires_at=CASE WHEN $3='requires_stop' THEN clock_timestamp()+interval '30 seconds' ELSE LEAST(clock_timestamp()+interval '30 seconds',deadline_at) END,closed_at=CASE WHEN $3='closed' THEN clock_timestamp() ELSE NULL END,updated_at=clock_timestamp() WHERE environment_id=$1 AND operation_id=$2")
        .bind(state.env.env_id).bind(row.operation_id).bind(next_state).bind(next)
        .execute(&mut *conn).await.map_err(unavailable)?;
    refresh_accounting(&mut *conn, state, row.operation_id)
        .await
        .map_err(unavailable)?;
    let row = live(conn, state, row.operation_id).await?;
    Ok(live_reply(&row, None))
}

// 条目的请求入口和实际目标均按当前授权重查；已接受的尾量仍由 append 读取历史快照。
async fn entry_model_current(
    conn: &mut PgConnection,
    state: &AppState,
    current: &Principal,
    submitted: &BeginRequest,
    entry: &NativeEntry,
) -> Result<bool, ApiError> {
    let mut target = submitted.clone();
    target.origin.model_id = entry.model_id;
    target.requested_model_id = entry.requested_model_id;
    target.requested_model = entry.requested_model.clone();
    target.upstream_model = entry.upstream_model.clone();
    origin_for_consumer(
        &mut *conn,
        state,
        Some(current),
        &target,
        Some(EndpointFamily::Realtime),
    )
    .await?;
    let actual: Value = sqlx::query_scalar(
        "SELECT doc FROM gw_resources WHERE kind='models' AND id=$1 AND NOT deleted",
    )
    .bind(entry.model_id)
    .fetch_optional(conn)
    .await
    .map_err(unavailable)?
    .ok_or_else(ApiError::not_found)?;
    Ok(actual.get("model_name").and_then(Value::as_str) == Some(entry.upstream_model.as_str()))
}

async fn accept_entries(
    conn: &mut PgConnection,
    state: &AppState,
    current: &Principal,
    submitted: &BeginRequest,
    entries: &[NativeEntry],
) -> Result<(), ApiError> {
    if entries.len() > MAX_ENTRIES {
        return Err(invalid("/entries"));
    }
    let mut seen = BTreeSet::new();
    for entry in entries {
        if entry.entry_id.is_empty() {
            if entry.model_id != submitted.origin.model_id
                || entry.requested_model_id != submitted.requested_model_id
                || entry.upstream_model != submitted.upstream_model
            {
                return Err(invalid("/entries"));
            }
            continue;
        }
        if entry.entry_id.is_empty()
            || !text_ok(&entry.entry_id, 256)
            || !seen.insert(entry.entry_id.as_str())
            || !text_ok(&entry.upstream_model, 256)
            || entry.endpoint != "/v1/audio/transcriptions"
        {
            return Err(invalid("/entries"));
        }
        let encoded = serde_json::to_value(entry).map_err(unavailable)?;
        let previous:Option<Value>=sqlx::query_scalar("SELECT entry FROM native_live_entries WHERE environment_id=$1 AND operation_id=$2 AND entry_id=$3")
            .bind(state.env.env_id).bind(submitted.operation_id).bind(&entry.entry_id)
            .fetch_optional(&mut *conn).await.map_err(unavailable)?;
        if previous
            .as_ref()
            .is_some_and(|previous| previous != &encoded)
        {
            return Err(conflict());
        }
        if !entry_model_current(&mut *conn, state, current, submitted, entry).await? {
            return Err(invalid("/entries/upstream_model"));
        }
        if previous.is_some() {
            continue;
        }
        sqlx::query("INSERT INTO native_live_entries(environment_id,operation_id,entry_id,entry) VALUES ($1,$2,$3,$4)")
            .bind(state.env.env_id).bind(submitted.operation_id).bind(&entry.entry_id).bind(encoded)
            .execute(&mut *conn).await.map_err(unavailable)?;
    }
    Ok(())
}

pub(super) async fn claim(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: ClaimLiveSessionsRequest,
) -> Result<NativeReply, ApiError> {
    if body.limit == 0 || body.limit > MAX_PAGE {
        return Err(invalid("/limit"));
    }
    let ids:Vec<Uuid>=sqlx::query_scalar("SELECT operation_id FROM native_live_sessions WHERE environment_id=$1 AND state<>'closed' AND lease_expires_at<=clock_timestamp() ORDER BY lease_expires_at,operation_id LIMIT $2 FOR UPDATE SKIP LOCKED")
        .bind(state.env.env_id).bind(body.limit as i64).fetch_all(&mut *conn).await.map_err(unavailable)?;
    let mut reply = NativeReply::new("claimed");
    for id in ids {
        sqlx::query("UPDATE native_live_sessions SET state='requires_stop',coverage='unknown',accounting_state='awaiting_reconciliation',lease_dp_id=$3,lease_generation=lease_generation+1,lease_expires_at=clock_timestamp()+interval '30 seconds',updated_at=clock_timestamp() WHERE environment_id=$1 AND operation_id=$2")
            .bind(state.env.env_id).bind(id).bind(dp.dp_id).execute(&mut *conn).await.map_err(unavailable)?;
        let mut record = live_record(&live(&mut *conn, state, id).await?);
        record.accepted = Some(submission(&operation(&mut *conn, state, id, false).await?)?);
        reply.live_sessions.push(record);
    }
    Ok(reply)
}

struct SipBinding {
    provider_key_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    signing_key: Vec<u8>,
    namespace: (String, String),
}

async fn sip_binding(
    conn: &mut PgConnection,
    state: &AppState,
    route: &str,
    lock: bool,
) -> Result<SipBinding, ApiError> {
    let rows: Vec<(Uuid, Value)> = sqlx::query_as(&format!(
        "SELECT id,doc FROM gw_resources WHERE kind='provider_keys' AND NOT deleted ORDER BY id{}",
        if lock { " FOR SHARE" } else { "" }
    ))
    .fetch_all(conn)
    .await
    .map_err(unavailable)?;
    let mut bindings = Vec::new();
    for (provider_key_id, mut doc) in rows {
        if doc.get("realtime_sip").is_none_or(Value::is_null) {
            continue;
        }
        secrets::unseal(
            &state.cfg.master_key,
            Kind::ProviderKeys,
            provider_key_id,
            &mut doc,
        )
        .map_err(unavailable)?;
        let binding = doc.get("realtime_sip").ok_or_else(ApiError::not_found)?;
        let text = |field: &str| {
            binding
                .get(field)
                .and_then(Value::as_str)
                .ok_or_else(ApiError::not_found)
        };
        let route_id = text("route_id")?.to_owned();
        let api_key_id = Uuid::parse_str(text("api_key_id")?).map_err(|_| ApiError::not_found())?;
        let model_id = Uuid::parse_str(text("model_id")?).map_err(|_| ApiError::not_found())?;
        let provider: ProviderKey = serde_json::from_value(doc.clone()).map_err(unavailable)?;
        let signing_key = provider
            .realtime_sip
            .as_ref()
            .ok_or_else(ApiError::not_found)?
            .signing_key()
            .map_err(|_| ApiError::not_found())?;
        let namespace = provider
            .realtime_sip_namespace()
            .map_err(|_| ApiError::not_found())?;
        bindings.push((
            route_id,
            SipBinding {
                provider_key_id,
                api_key_id,
                model_id,
                signing_key,
                namespace,
            },
        ));
    }
    let Some(index) = bindings.iter().position(|(id, _)| id == route) else {
        return Err(ApiError::not_found());
    };
    let selected = &bindings[index];
    if bindings.iter().enumerate().any(|(other, (id, binding))| {
        other != index
            && (id == route
                || binding.signing_key == selected.1.signing_key
                || binding.namespace == selected.1.namespace)
    }) {
        // 即使旧配置绕过了写入校验，同一个签名密钥也不能跨租户路由认领。
        return Err(ApiError::not_found());
    }
    Ok(bindings.swap_remove(index).1)
}

fn sip_signature_valid(
    binding: &SipBinding,
    body: &SignedSipIntakeRequest,
    at: DateTime<Utc>,
) -> bool {
    // 官方 SDK: openai-python 3dae6090/src/openai/lib/_webhooks.py。
    let Ok(timestamp) = body.webhook_timestamp.parse::<i64>() else {
        return false;
    };
    if at.timestamp().abs_diff(timestamp) > 300 {
        return false;
    }
    let signed = format!(
        "{}.{}.{}",
        body.webhook_id, body.webhook_timestamp, body.raw_body
    );
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &binding.signing_key);
    body.webhook_signature.split_whitespace().any(|signature| {
        let signature = signature.strip_prefix("v1,").unwrap_or(signature);
        base64::engine::general_purpose::STANDARD
            .decode(signature)
            .is_ok_and(|signature| ring::hmac::verify(&key, signed.as_bytes(), &signature).is_ok())
    })
}

pub(super) async fn sip_intake(
    conn: &mut PgConnection,
    state: &AppState,
    dp: DpIdentity,
    body: SignedSipIntakeRequest,
) -> Result<NativeReply, ApiError> {
    if !text_ok(&body.route_id, 64)
        || !body
            .route_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || !text_ok(&body.webhook_id, 512)
        || body.raw_body.len() > 256 * 1024
        || body.webhook_signature.len() > 4096
    {
        return Err(ApiError::not_found());
    }
    let candidate = sip_binding(&mut *conn, state, &body.route_id, false).await?;
    let current = realtime_principal(&mut *conn, candidate.api_key_id).await?;
    let binding = sip_binding(&mut *conn, state, &body.route_id, true).await?;
    if binding.api_key_id != candidate.api_key_id
        || binding.model_id != candidate.model_id
        || binding.provider_key_id != candidate.provider_key_id
    {
        return Err(ApiError::not_found());
    }
    let at = now(&mut *conn).await?;
    if !sip_signature_valid(&binding, &body, at) {
        return Err(ApiError::not_found());
    }
    let signed: Value = serde_json::from_str(&body.raw_body).map_err(|_| ApiError::not_found())?;
    let provider_event_id = signed
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| text_ok(value, 512))
        .ok_or_else(ApiError::not_found)?;
    let call_id = signed
        .pointer("/data/call_id")
        .and_then(Value::as_str)
        .filter(|value| text_ok(value, 512))
        .ok_or_else(ApiError::not_found)?;
    if signed.get("type").and_then(Value::as_str) != Some("realtime.call.incoming") {
        return Err(ApiError::not_found());
    }
    let op = operation(&mut *conn, state, body.operation_id, true).await?;
    let submitted = submission(&op)?;
    require_live_submission(&submitted)?;
    if op.dp_id != dp.dp_id
        || op.api_key_id != current.key.id
        || op.tenant_id != current.tenant.id
        || submitted.origin.provider_key_id != binding.provider_key_id
        || submitted.origin.model_id != binding.model_id
        || submitted.requested_model_id != binding.model_id
    {
        return Err(ApiError::not_found());
    }
    origin_for_consumer(
        &mut *conn,
        state,
        Some(&current),
        &submitted,
        Some(EndpointFamily::Realtime),
    )
    .await?;
    // 所有同账号事件和 call 的竞争都经同一事务锁；绝不从 SIP header 推导 owner。
    let identity = format!(
        "native-sip:{}:{}",
        state.env.env_id,
        submitted.origin.boundary.as_str()
    );
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(identity)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    let old:Vec<(Uuid,String,String,String)>=sqlx::query_as("SELECT operation_id,route_id,provider_event_id,call_id FROM native_sip_intakes WHERE environment_id=$1 AND boundary=$2 AND (provider_event_id=$3 OR call_id=$4 OR webhook_id=$5)")
        .bind(state.env.env_id).bind(submitted.origin.boundary.as_str()).bind(provider_event_id).bind(call_id).bind(&body.webhook_id)
        .fetch_all(&mut *conn).await.map_err(unavailable)?;
    if let Some((operation_id, route, event, call)) = old.first() {
        if old.len() != 1
            || route != &body.route_id
            || event != provider_event_id
            || call != call_id
        {
            return Err(conflict());
        }
        let row = live(conn, state, *operation_id).await?;
        let mut reply = live_reply(&row, Some(false));
        reply.operation_id = Some(*operation_id);
        return Ok(reply);
    }
    let mut reply = create_inner(
        &mut *conn,
        state,
        dp,
        CreateLiveSessionRequest {
            operation_id: body.operation_id,
            api_key_id: binding.api_key_id,
            purpose: RealtimePurpose::Realtime,
            transport: RealtimeTransport::Sip,
            credential_id: None,
            provider_session_id: Some(call_id.into()),
            deadline_at: body.deadline_at,
        },
        true,
    )
    .await?;
    let bound = bind(
        &mut *conn,
        state,
        dp,
        BindRequest {
            existing_record_id: None,
            objects: Vec::new(),
            operation_id: body.operation_id,
            api_key_id: binding.api_key_id,
            raw_id: call_id.into(),
            provider_created_at: None,
            provider_expires_at: None,
        },
    )
    .await?;
    sqlx::query("INSERT INTO native_sip_intakes(environment_id,boundary,provider_event_id,webhook_id,call_id,route_id,provider_key_id,operation_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(state.env.env_id).bind(submitted.origin.boundary.as_str()).bind(provider_event_id).bind(body.webhook_id)
        .bind(call_id).bind(body.route_id).bind(binding.provider_key_id).bind(body.operation_id)
        .execute(conn).await.map_err(unavailable)?;
    reply.record = bound.record;
    reply.appended = Some(true);
    Ok(reply)
}

async fn refresh_accounting(
    conn: &mut PgConnection,
    state: &AppState,
    operation_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE native_live_sessions session SET accounting_state=CASE WHEN session.coverage='unknown' OR (session.state='closed' AND session.coverage<>'complete') OR EXISTS(SELECT 1 FROM native_live_journal journal WHERE journal.environment_id=session.environment_id AND journal.operation_id=session.operation_id AND jsonb_array_length(journal.usage)>(SELECT count(*) FROM native_live_outbox outbox WHERE outbox.journal_id=journal.id)) OR EXISTS(SELECT 1 FROM native_live_journal journal JOIN native_live_outbox outbox ON outbox.journal_id=journal.id WHERE journal.environment_id=session.environment_id AND journal.operation_id=session.operation_id AND outbox.pricing_state='unpriced') THEN 'awaiting_reconciliation' WHEN session.state='closed' AND session.coverage='complete' AND EXISTS(SELECT 1 FROM native_live_journal journal JOIN native_live_outbox outbox ON outbox.journal_id=journal.id WHERE journal.environment_id=session.environment_id AND journal.operation_id=session.operation_id) AND NOT EXISTS(SELECT 1 FROM native_live_journal journal JOIN native_live_outbox outbox ON outbox.journal_id=journal.id WHERE journal.environment_id=session.environment_id AND journal.operation_id=session.operation_id AND outbox.delivered_at IS NULL) THEN 'settled' ELSE session.accounting_state END,updated_at=clock_timestamp() WHERE session.environment_id=$1 AND session.operation_id=$2")
        .bind(state.env.env_id).bind(operation_id).execute(conn).await?;
    Ok(())
}

/// 与既有接收器同事务确认，CP 重启或普通 usage 保留期不删除去重依据。
pub(crate) async fn deliver_one(
    conn: &mut PgConnection,
    state: &AppState,
) -> Result<bool, DbError> {
    let rows:Vec<(Uuid,Uuid,Value)>=sqlx::query_as("SELECT event_id,dp_id,event FROM native_live_outbox WHERE delivered_at IS NULL AND dp_id=(SELECT dp_id FROM native_live_outbox WHERE delivered_at IS NULL ORDER BY created_at,event_id LIMIT 1) ORDER BY created_at,event_id LIMIT 100 FOR UPDATE SKIP LOCKED")
        .fetch_all(&mut *conn).await?;
    let Some((batch_id, dp_id, _)) = rows.first() else {
        return Ok(false);
    };
    let (batch_id, dp_id) = (*batch_id, *dp_id);
    let ids: Vec<Uuid> = rows.iter().map(|(id, _, _)| *id).collect();
    let events = rows.into_iter().map(|(_, _, event)| event).collect();
    let outcome =
        crate::usage::ingest_transaction(&mut *conn, &state.prices, dp_id, batch_id, events)
            .await?;
    if outcome.rejected != 0 {
        return Err(DbError::Db(sqlx::Error::Protocol(
            "native live event was rejected".into(),
        )));
    }
    let delivered=sqlx::query("UPDATE native_live_outbox outbox SET delivered_at=clock_timestamp(),pricing_state=charge.status FROM pricing_charges_cny charge WHERE outbox.event_id=ANY($1) AND charge.event_id=outbox.event_id::text")
        .bind(&ids).execute(&mut *conn).await?;
    if delivered.rows_affected() != ids.len() as u64 {
        return Err(DbError::Db(sqlx::Error::Protocol(
            "native live pricing receipt is missing".into(),
        )));
    }
    let operations:Vec<Uuid>=sqlx::query_scalar("SELECT DISTINCT journal.operation_id FROM native_live_journal journal JOIN native_live_outbox outbox ON outbox.journal_id=journal.id WHERE outbox.event_id=ANY($1)")
        .bind(&ids).fetch_all(&mut *conn).await?;
    for operation_id in operations {
        refresh_accounting(&mut *conn, state, operation_id).await?;
    }
    Ok(true)
}
