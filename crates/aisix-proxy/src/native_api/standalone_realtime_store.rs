//! 实时凭据、会话和不可变本地日志；不保存供应商令牌，不增加计费投递器。
use std::collections::BTreeSet;

use aisix_core::{
    models::EndpointFamily,
    native_metering::{validate_observed_usage, ObservedMeter, ObservedUsage},
    native_objects::*,
    native_realtime::*,
    AisixSnapshot, ProviderKey,
};
use base64::Engine as _;
use chrono::{DateTime, TimeZone, Utc};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use sqlx::{sqlite::SqliteRow, Row, SqliteConnection};
use uuid::Uuid;

use super::{
    registry::NativeRegistryError as Error,
    standalone_store::{stable_resource_id, Owner, Principal, StandaloneNativeStore},
};

type Result<T> = std::result::Result<T, Error>;
const LIVE_LEASE_MS: i64 = 30_000;
const MAX_ENTRIES: usize = 1000;

fn unavailable<T>(_: T) -> Error {
    Error::Unavailable
}
fn encode<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(unavailable)
}
fn decode<T: DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(unavailable)
}
fn now() -> i64 {
    Utc::now().timestamp_millis()
}
fn timestamp(value: i64) -> Result<DateTime<Utc>> {
    Utc.timestamp_millis_opt(value)
        .single()
        .ok_or(Error::Unavailable)
}
fn parse_time(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|t| t.with_timezone(&Utc))
        .map_err(unavailable)
}
fn uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(unavailable)
}
fn text_ok(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn purpose(value: RealtimePurpose) -> Result<&'static str> {
    match value {
        RealtimePurpose::Realtime => Ok("realtime"),
        RealtimePurpose::Transcription => Ok("transcription"),
        RealtimePurpose::Translation => Ok("translation"),
        RealtimePurpose::Unknown => Err(Error::InvalidRequest),
    }
}
fn transport(value: RealtimeTransport) -> Result<&'static str> {
    match value {
        RealtimeTransport::WebSocket => Ok("web_socket"),
        RealtimeTransport::WebRtc => Ok("web_rtc"),
        RealtimeTransport::Sip => Ok("sip"),
        RealtimeTransport::Unknown => Err(Error::InvalidRequest),
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
        "partial" => UsageCoverage::Partial,
        "complete" => UsageCoverage::Complete,
        _ => UsageCoverage::Unknown,
    }
}
fn token_hash_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub(super) async fn initialize(conn: &mut SqliteConnection) -> Result<()> {
    sqlx::raw_sql(include_str!("standalone_realtime_store.sql"))
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    sqlx::query(
        "INSERT INTO native_meta(name,value) VALUES('realtime_schema','1') ON CONFLICT DO NOTHING",
    )
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    let version: String =
        sqlx::query_scalar("SELECT value FROM native_meta WHERE name='realtime_schema'")
            .fetch_one(&mut *conn)
            .await
            .map_err(unavailable)?;
    if version != "1" {
        return Err(Error::Unavailable);
    }
    Ok(())
}

// Schema 内的 value/token 可以是合法属性名；只检查真实默认配置凭据槽。
fn metadata_safe(value: &Value, usage_only: bool) -> bool {
    if !usage_only {
        let safe = |v: &Value| {
            v.is_null()
                || v.as_object().is_some_and(|fields| {
                    !fields.keys().any(|name| {
                        matches!(
                            name.to_ascii_lowercase().as_str(),
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
        return safe(value) && value.get("session").is_none_or(safe);
    }
    match value {
        Value::Object(fields) => fields.iter().all(|(name, v)| {
            !matches!(
                name.to_ascii_lowercase().as_str(),
                "client_secret"
                    | "api_key"
                    | "access_token"
                    | "refresh_token"
                    | "authorization"
                    | "password"
                    | "secret"
                    | "token"
                    | "value"
                    | "audio"
                    | "text"
                    | "transcript"
                    | "delta"
                    | "instructions"
                    | "session"
                    | "sdp"
            ) && metadata_safe(v, true)
        }),
        Value::Array(values) => values.iter().all(|v| metadata_safe(v, true)),
        _ => true,
    }
}

async fn operation(conn: &mut SqliteConnection, id: Uuid) -> Result<(BeginRequest, Owner, Uuid)> {
    let row = sqlx::query(
        "SELECT submission,caller,worker_id FROM native_operations WHERE operation_id=?",
    )
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(unavailable)?
    .ok_or(Error::NotFound)?;
    let submitted: BeginRequest = decode(row.try_get("submission").map_err(unavailable)?)?;
    if submitted.protocol != NativeProtocol::Openai
        || submitted.kind != NativeObjectKind::RealtimeCall
    {
        return Err(Error::InvalidRequest);
    }
    Ok((
        submitted,
        decode(row.try_get("caller").map_err(unavailable)?)?,
        uuid(row.try_get("worker_id").map_err(unavailable)?)?,
    ))
}

fn current_principal(snapshot: &AisixSnapshot, key_id: Uuid) -> Result<Principal> {
    let principal = StandaloneNativeStore::principal(snapshot, key_id)?;
    if !principal.key.value.can_call(EndpointFamily::Realtime) {
        return Err(Error::NotFound);
    }
    Ok(principal)
}

fn current_operation(
    store: &StandaloneNativeStore,
    snapshot: &AisixSnapshot,
    principal: &Principal,
    submitted: &BeginRequest,
    owner: &Owner,
    _worker: Uuid,
) -> Result<()> {
    if submitted.api_key_id != stable_resource_id("apikeys", &principal.key.id)
        || *owner != principal.owner
    {
        return Err(Error::NotFound);
    }
    store.authorize_origin(
        snapshot,
        Some(principal),
        submitted,
        NativeObjectKind::RealtimeCall,
        Some(EndpointFamily::Realtime),
    )
}

struct Credential {
    id: Uuid,
    submitted: BeginRequest,
    key_id: String,
    owner: Owner,
    key_hash: String,
    purpose: String,
    expires_at: DateTime<Utc>,
    defaults: Value,
    revoked: bool,
}
async fn credential(
    conn: &mut SqliteConnection,
    selector: &str,
    value: &str,
) -> Result<Credential> {
    let query = match selector {
        "token_hash" => "SELECT * FROM native_realtime_credentials WHERE token_hash=?",
        _ => "SELECT * FROM native_realtime_credentials WHERE credential_id=?",
    };
    let row = sqlx::query(query)
        .bind(value)
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?
        .ok_or(Error::NotFound)?;
    let operation_id = uuid(row.try_get("operation_id").map_err(unavailable)?)?;
    let submitted = operation(conn, operation_id).await?.0;
    Ok(Credential {
        id: uuid(row.try_get("credential_id").map_err(unavailable)?)?,
        submitted,
        key_id: row.try_get("key_id").map_err(unavailable)?,
        owner: decode(row.try_get("owner").map_err(unavailable)?)?,
        key_hash: row.try_get("creator_key_hash").map_err(unavailable)?,
        purpose: row.try_get("purpose").map_err(unavailable)?,
        expires_at: parse_time(row.try_get("expires_at").map_err(unavailable)?)?,
        defaults: decode(row.try_get("defaults").map_err(unavailable)?)?,
        revoked: row
            .try_get::<Option<i64>, _>("revoked_at")
            .map_err(unavailable)?
            .is_some(),
    })
}
fn credential_current(row: &Credential, current: &Principal, desired: &str) -> Result<()> {
    if row.key_id != current.key.id
        || row.owner != current.owner
        || row.key_hash != current.key.value.key_hash
        || row.purpose != desired
        || row.expires_at <= Utc::now()
        || row.revoked
    {
        return Err(Error::NotFound);
    }
    Ok(())
}
fn credential_reply(row: Credential) -> NativeReply {
    let mut reply = NativeReply::new("credential");
    reply.credential = Some(NativeCredential {
        credential_id: row.id,
        api_key_id: stable_resource_id("apikeys", &row.key_id),
        purpose: parse_purpose(&row.purpose),
        expires_at: Some(row.expires_at),
        defaults: row.defaults,
        origin: Some(row.submitted.origin),
        requested_model_id: row.submitted.requested_model_id,
        requested_model: row.submitted.requested_model,
    });
    reply
}

pub(super) async fn mint_credential(
    store: &StandaloneNativeStore,
    body: MintCredentialRequest,
) -> Result<NativeReply> {
    let desired = purpose(body.purpose)?;
    if !token_hash_valid(&body.token_hash)
        || !metadata_safe(&body.defaults, false)
        || encode(&body.defaults)?.len() > 256 * 1024
        || body.expires_at <= Utc::now()
        || body.expires_at > Utc::now() + chrono::Duration::hours(2)
    {
        return Err(Error::InvalidRequest);
    }
    let mut tx = store.write_tx().await?;
    let snapshot = store.snapshot.load();
    let principal = current_principal(&snapshot, body.api_key_id)?;
    if body.expires_at <= Utc::now() || body.expires_at > Utc::now() + chrono::Duration::hours(2) {
        return Err(Error::InvalidRequest);
    }
    let (submitted, owner, worker) = operation(&mut tx, body.operation_id).await?;
    current_operation(store, &snapshot, &principal, &submitted, &owner, worker)?;
    let prior: Option<String> = sqlx::query_scalar(
        "SELECT token_hash FROM native_realtime_credentials WHERE operation_id=?",
    )
    .bind(body.operation_id.to_string())
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;
    if let Some(hash) = prior {
        let row = credential(&mut tx, "token_hash", &hash).await?;
        credential_current(&row, &principal, desired)?;
        if hash != body.token_hash
            || row.expires_at != body.expires_at
            || row.defaults != body.defaults
        {
            return Err(Error::Conflict);
        }
        return Ok(credential_reply(row));
    }
    let duplicate: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_realtime_credentials WHERE token_hash=?")
            .bind(&body.token_hash)
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?;
    if duplicate != 0 {
        return Err(Error::Conflict);
    }
    sqlx::query("INSERT INTO native_realtime_credentials(credential_id,operation_id,token_hash,key_id,owner,creator_key_hash,purpose,expires_at,expiry_ms,defaults) VALUES(?,?,?,?,?,?,?,?,?,?)")
        .bind(Uuid::new_v4().to_string()).bind(body.operation_id.to_string()).bind(&body.token_hash).bind(&principal.key.id).bind(encode(&principal.owner)?).bind(&principal.key.value.key_hash).bind(desired).bind(body.expires_at.to_rfc3339()).bind(body.expires_at.timestamp_millis()).bind(encode(&body.defaults)?).execute(&mut *tx).await.map_err(unavailable)?;
    let row = credential(&mut tx, "token_hash", &body.token_hash).await?;
    tx.commit().await.map_err(unavailable)?;
    Ok(credential_reply(row))
}

pub(super) async fn redeem_credential(
    store: &StandaloneNativeStore,
    body: RedeemCredentialRequest,
) -> Result<NativeReply> {
    let desired = purpose(body.purpose).map_err(|_| Error::NotFound)?;
    if !token_hash_valid(&body.token_hash)
        || body
            .client_ip
            .as_ref()
            .is_some_and(|ip| ip.parse::<std::net::IpAddr>().is_err())
    {
        return Err(Error::NotFound);
    }
    let mut tx = store.write_tx().await?;
    let row = credential(&mut tx, "token_hash", &body.token_hash).await?;
    let snapshot = store.snapshot.load();
    let principal = current_principal(&snapshot, stable_resource_id("apikeys", &row.key_id))?;
    credential_current(&row, &principal, desired)?;
    // 默认模型只用于审计；最终 override 仍通过新的 begin/create 当前授权。
    tx.commit().await.map_err(unavailable)?;
    Ok(credential_reply(row))
}

struct Live {
    operation_id: Uuid,
    api_key_id: Uuid,
    owner: Owner,
    credential_id: Option<Uuid>,
    purpose: String,
    transport: String,
    provider_session_id: Option<String>,
    state: String,
    coverage: String,
    accounting: String,
    worker: Uuid,
    generation: i64,
    lease_expires_at: i64,
    deadline: DateTime<Utc>,
}
fn live_from_row(row: SqliteRow) -> Result<Live> {
    Ok(Live {
        operation_id: uuid(row.try_get("operation_id").map_err(unavailable)?)?,
        api_key_id: uuid(row.try_get("api_key_id").map_err(unavailable)?)?,
        owner: decode(row.try_get("owner").map_err(unavailable)?)?,
        credential_id: row
            .try_get::<Option<String>, _>("credential_id")
            .map_err(unavailable)?
            .map(|id| uuid(&id))
            .transpose()?,
        purpose: row.try_get("purpose").map_err(unavailable)?,
        transport: row.try_get("transport").map_err(unavailable)?,
        provider_session_id: row.try_get("provider_session_id").map_err(unavailable)?,
        state: row.try_get("state").map_err(unavailable)?,
        coverage: row.try_get("coverage").map_err(unavailable)?,
        accounting: row.try_get("accounting_state").map_err(unavailable)?,
        worker: uuid(row.try_get("lease_worker").map_err(unavailable)?)?,
        generation: row.try_get("lease_generation").map_err(unavailable)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(unavailable)?,
        deadline: parse_time(row.try_get("deadline_at").map_err(unavailable)?)?,
    })
}
async fn live(conn: &mut SqliteConnection, id: Uuid) -> Result<Live> {
    live_from_row(
        sqlx::query("SELECT * FROM native_live_sessions WHERE operation_id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *conn)
            .await
            .map_err(unavailable)?
            .ok_or(Error::NotFound)?,
    )
}
fn live_record(row: &Live) -> Result<LiveSessionRecord> {
    Ok(LiveSessionRecord {
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
            generation: row.generation,
        }),
        lease_expires_at: Some(timestamp(row.lease_expires_at)?),
        deadline_at: Some(row.deadline),
        state: row.state.clone(),
        coverage: parse_coverage(&row.coverage),
        accounting_state: row.accounting.clone(),
        requires_stop: row.state == "requires_stop",
        accepted: None,
    })
}
fn live_reply(row: &Live, appended: Option<bool>) -> Result<NativeReply> {
    let mut reply = NativeReply::new(&row.state);
    reply.operation_id = Some(row.operation_id);
    reply.metering_state = Some(row.accounting.clone());
    reply.reconciliation_reason =
        (row.coverage == "unknown").then(|| "incomplete_observation".into());
    reply.live_session = Some(live_record(row)?);
    reply.appended = appended;
    Ok(reply)
}

pub(super) async fn create_live_session(
    store: &StandaloneNativeStore,
    body: CreateLiveSessionRequest,
) -> Result<NativeReply> {
    let mut tx = store.write_tx().await?;
    let reply = create_inner(store, &mut tx, body, false).await?;
    tx.commit().await.map_err(unavailable)?;
    Ok(reply)
}
async fn create_inner(
    store: &StandaloneNativeStore,
    conn: &mut SqliteConnection,
    body: CreateLiveSessionRequest,
    verified_sip: bool,
) -> Result<NativeReply> {
    let desired = purpose(body.purpose)?;
    let wire_transport = transport(body.transport)?;
    if body
        .provider_session_id
        .as_ref()
        .is_some_and(|id| !text_ok(id, 512))
        || (body.transport == RealtimeTransport::Sip && !verified_sip)
    {
        return Err(Error::InvalidRequest);
    }
    let snapshot = store.snapshot.load();
    let current = current_principal(&snapshot, body.api_key_id)?;
    let (submitted, owner, worker) = operation(conn, body.operation_id).await?;
    current_operation(store, &snapshot, &current, &submitted, &owner, worker)?;
    let prior = sqlx::query("SELECT * FROM native_live_sessions WHERE operation_id=?")
        .bind(body.operation_id.to_string())
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?;
    if let Some(prior) = prior {
        let row = live_from_row(prior)?;
        if row.api_key_id != body.api_key_id
            || row.owner != current.owner
            || row.credential_id != body.credential_id
            || row.purpose != desired
            || row.transport != wire_transport
            || row.provider_session_id != body.provider_session_id
            || row.deadline != body.deadline_at
        {
            return Err(Error::Conflict);
        }
        return live_reply(&row, None);
    }
    if body.deadline_at <= Utc::now() {
        return Err(Error::InvalidRequest);
    }
    if let Some(id) = body.credential_id {
        let credential = credential(conn, "credential_id", &id.to_string()).await?;
        credential_current(&credential, &current, desired)?;
    }
    sqlx::query("INSERT INTO native_live_sessions(operation_id,api_key_id,owner,credential_id,purpose,transport,provider_session_id,state,coverage,accounting_state,lease_worker,lease_generation,lease_expires_at,deadline_at,deadline_ms) VALUES(?,?,?,?,?,?,?,'live','partial','pending',?,1,?,?,?)")
        .bind(body.operation_id.to_string()).bind(body.api_key_id.to_string()).bind(encode(&current.owner)?).bind(body.credential_id.map(|id|id.to_string())).bind(desired).bind(wire_transport).bind(body.provider_session_id).bind(store.worker_id.to_string()).bind((now()+LIVE_LEASE_MS).min(body.deadline_at.timestamp_millis())).bind(body.deadline_at.to_rfc3339()).bind(body.deadline_at.timestamp_millis()).execute(&mut *conn).await.map_err(unavailable)?;
    accept_entries(
        store,
        conn,
        &snapshot,
        &current,
        &submitted,
        &submitted.entries,
    )
    .await?;
    live_reply(&live(conn, body.operation_id).await?, None)
}

fn lease_owner(
    store: &StandaloneNativeStore,
    row: &Live,
    lease: Option<&LiveSessionLease>,
) -> Result<()> {
    // 成功 open 已证明同一受保护 DB/稳定 secret；显式当前租约不绑定进程 UUID。
    if lease.is_some_and(|lease| {
        lease.operation_id != row.operation_id || lease.generation != row.generation
    }) || (lease.is_none() && (row.generation != 1 || row.worker != store.worker_id))
    {
        return Err(Error::NotFound);
    }
    Ok(())
}

fn entry_current(
    store: &StandaloneNativeStore,
    snapshot: &AisixSnapshot,
    current: &Principal,
    submitted: &BeginRequest,
    entry: &NativeEntry,
) -> Result<()> {
    let mut target = submitted.clone();
    target.origin.model_id = entry.model_id;
    target.requested_model_id = entry.requested_model_id;
    target.requested_model = entry.requested_model.clone();
    target.upstream_model = entry.upstream_model.clone();
    store.authorize_origin(
        snapshot,
        Some(current),
        &target,
        NativeObjectKind::RealtimeCall,
        Some(EndpointFamily::Realtime),
    )?;
    let mut matches = snapshot
        .models
        .entries()
        .into_iter()
        .filter(|model| stable_resource_id("models", &model.id) == entry.model_id);
    let model = matches.next().ok_or(Error::NotFound)?;
    if matches.next().is_some()
        || model.value.model_name.as_deref() != Some(entry.upstream_model.as_str())
    {
        return Err(Error::NotFound);
    }
    Ok(())
}
async fn accept_entries(
    store: &StandaloneNativeStore,
    conn: &mut SqliteConnection,
    snapshot: &AisixSnapshot,
    current: &Principal,
    submitted: &BeginRequest,
    entries: &[NativeEntry],
) -> Result<()> {
    if entries.len() > MAX_ENTRIES {
        return Err(Error::InvalidRequest);
    }
    let mut seen = BTreeSet::new();
    for entry in entries {
        if entry.entry_id.is_empty() {
            if entry.model_id != submitted.origin.model_id
                || entry.requested_model_id != submitted.requested_model_id
                || entry.upstream_model != submitted.upstream_model
            {
                return Err(Error::InvalidRequest);
            }
            continue;
        }
        if !text_ok(&entry.entry_id, 256)
            || !seen.insert(&entry.entry_id)
            || !text_ok(&entry.upstream_model, 256)
            || entry.endpoint != "/v1/audio/transcriptions"
        {
            return Err(Error::InvalidRequest);
        }
        let encoded = encode(entry)?;
        if encoded.len() > 16 * 1024 {
            return Err(Error::InvalidRequest);
        }
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT entry FROM native_live_entries WHERE operation_id=? AND entry_id=?",
        )
        .bind(submitted.operation_id.to_string())
        .bind(&entry.entry_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?;
        if previous
            .as_ref()
            .is_some_and(|previous| previous != &encoded)
        {
            return Err(Error::Conflict);
        }
        entry_current(store, snapshot, current, submitted, entry)?;
        if previous.is_some() {
            continue;
        }
        sqlx::query("INSERT INTO native_live_entries(operation_id,entry_id,entry) VALUES(?,?,?)")
            .bind(submitted.operation_id.to_string())
            .bind(&entry.entry_id)
            .bind(encoded)
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
    }
    Ok(())
}

fn normalized_measurement(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(entries) = value.as_array_mut() {
        entries.sort_by(|a, b| {
            a.get("entry_id")
                .and_then(Value::as_str)
                .cmp(&b.get("entry_id").and_then(Value::as_str))
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
fn validate_usage(usage: &[ObservedUsage]) -> Result<()> {
    if usage.len()>MAX_ENTRIES || (!usage.is_empty() && validate_observed_usage(usage).is_err()) || usage.iter().any(|entry| !text_ok(&entry.upstream_model,256) || entry.entry_id.len()>256 || entry.entry_id.chars().any(char::is_control) || matches!(&entry.meter,ObservedMeter::Tokens{usage} if usage.cached_prompt_tokens>usage.prompt_tokens || !(100..=599).contains(&usage.status_code) || usage.error_class.len()>64 || usage.error_message.len()>4096 || usage.io_audit.as_ref().is_some_and(|audit|audit.version!=2 || !audit.validate()))) {return Err(Error::InvalidRequest);}
    Ok(())
}
fn known_tokens(submitted: &BeginRequest, usage: &ObservedUsage) -> bool {
    matches!(&usage.meter, ObservedMeter::Tokens { .. })
        && if usage.entry_id.is_empty() {
            usage.upstream_model == submitted.upstream_model
        } else {
            submitted.entries.iter().any(|entry| {
                entry.entry_id == usage.entry_id && entry.upstream_model == usage.upstream_model
            })
        }
}

pub(super) async fn append_live_usage(
    store: &StandaloneNativeStore,
    body: AppendLiveUsageRequest,
) -> Result<NativeReply> {
    if !text_ok(&body.event_id, 512)
        || body
            .response_id
            .as_ref()
            .is_some_and(|id| !text_ok(id, 512))
        || body
            .raw_usage
            .as_ref()
            .is_some_and(|value| !metadata_safe(value, true))
    {
        return Err(Error::InvalidRequest);
    }
    validate_usage(&body.usage)?;
    let usage = encode(&body.usage)?;
    let raw = body.raw_usage.as_ref().map(encode).transpose()?;
    if usage.len() > 1024 * 1024 || raw.as_ref().is_some_and(|raw| raw.len() > 256 * 1024) {
        return Err(Error::InvalidRequest);
    }
    let mut tx = store.write_tx().await?;
    let row = live(&mut tx, body.operation_id).await?;
    if row.api_key_id != body.api_key_id {
        return Err(Error::NotFound);
    }
    lease_owner(store, &row, body.lease.as_ref())?;
    let event_coverage = if body.terminal && body.usage.is_empty() {
        "unknown"
    } else {
        coverage(body.coverage)
    };
    let old=sqlx::query("SELECT event_id,response_id,usage,raw_usage,coverage,terminal FROM native_live_journal WHERE operation_id=? AND (event_id=? OR (? IS NOT NULL AND response_id=?))")
        .bind(body.operation_id.to_string()).bind(&body.event_id).bind(&body.response_id).bind(&body.response_id).fetch_all(&mut *tx).await.map_err(unavailable)?;
    if !old.is_empty() {
        let observed: Value = decode(&usage)?;
        for old in old {
            let old_event: &str = old.try_get("event_id").map_err(unavailable)?;
            let old_response: Option<String> = old.try_get("response_id").map_err(unavailable)?;
            let old_usage: Value = decode(old.try_get("usage").map_err(unavailable)?)?;
            let old_raw: Option<Value> = old
                .try_get::<Option<String>, _>("raw_usage")
                .map_err(unavailable)?
                .as_deref()
                .map(decode)
                .transpose()?;
            if (old_event == body.event_id && old_response != body.response_id)
                || normalized_measurement(&old_usage) != normalized_measurement(&observed)
                || old_raw != body.raw_usage
                || old.try_get::<&str, _>("coverage").map_err(unavailable)? != event_coverage
                || old.try_get::<bool, _>("terminal").map_err(unavailable)? != body.terminal
            {
                return Err(Error::Conflict);
            }
        }
        return live_reply(&row, Some(false));
    }
    // 已校验持久身份和代次；尾量不因 TTL 丢失，也不刷新租约或授予新工作。
    let mut submitted = operation(&mut tx, body.operation_id).await?.0;
    let entries: Vec<String> = sqlx::query_scalar(
        "SELECT entry FROM native_live_entries WHERE operation_id=? ORDER BY entry_id",
    )
    .bind(body.operation_id.to_string())
    .fetch_all(&mut *tx)
    .await
    .map_err(unavailable)?;
    submitted.entries = entries
        .iter()
        .map(|entry| decode(entry))
        .collect::<Result<Vec<_>>>()?;
    sqlx::query("INSERT INTO native_live_journal(journal_id,operation_id,event_id,response_id,observer_worker,usage,raw_usage,coverage,terminal,accepted,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?)")
        .bind(Uuid::new_v4().to_string()).bind(body.operation_id.to_string()).bind(&body.event_id).bind(&body.response_id).bind(store.worker_id.to_string()).bind(usage).bind(raw).bind(event_coverage).bind(body.terminal).bind(encode(&submitted)?).bind(now()).execute(&mut *tx).await.map_err(unavailable)?;
    let lost =
        row.coverage == "unknown" || event_coverage == "unknown" || row.state == "requires_stop";
    let unpriced = body
        .usage
        .iter()
        .any(|usage| !known_tokens(&submitted, usage));
    let accounting = if lost
        || unpriced
        || row.accounting == "awaiting_reconciliation"
        || (body.terminal && event_coverage != "complete")
    {
        "awaiting_reconciliation"
    } else if body.usage.is_empty() {
        row.accounting.as_str()
    } else {
        "observed"
    };
    let next_coverage = if lost { "unknown" } else { event_coverage };
    sqlx::query("UPDATE native_live_sessions SET state=CASE WHEN ? THEN 'closed' ELSE state END, closed_at=CASE WHEN ? THEN COALESCE(closed_at,?) ELSE closed_at END,coverage=?,accounting_state=? WHERE operation_id=?")
        .bind(body.terminal).bind(body.terminal).bind(now()).bind(next_coverage).bind(accounting).bind(body.operation_id.to_string()).execute(&mut *tx).await.map_err(unavailable)?;
    let reply = live_reply(&live(&mut tx, body.operation_id).await?, Some(true))?;
    tx.commit().await.map_err(unavailable)?;
    Ok(reply)
}

pub(super) async fn checkpoint_live_session(
    store: &StandaloneNativeStore,
    body: CheckpointLiveSessionRequest,
) -> Result<NativeReply> {
    let mut tx = store.write_tx().await?;
    let row = live(&mut tx, body.lease.operation_id).await?;
    lease_owner(store, &row, Some(&body.lease))?;
    if row.state == "closed" {
        return live_reply(&row, None);
    }
    let at = now();
    if row.lease_expires_at <= at {
        return Err(Error::NotFound);
    }
    let submitted = operation(&mut tx, row.operation_id).await?.0;
    let snapshot = store.snapshot.load();
    let mut authorization_lost = false;
    if !body.terminal {
        let authorized = async {
            let current = current_principal(&snapshot, row.api_key_id)?;
            if current.owner != row.owner {
                return Err(Error::NotFound);
            }
            store.authorize_origin(
                &snapshot,
                Some(&current),
                &submitted,
                NativeObjectKind::RealtimeCall,
                Some(EndpointFamily::Realtime),
            )?;
            let entries: Vec<String> = sqlx::query_scalar(
                "SELECT entry FROM native_live_entries WHERE operation_id=? ORDER BY entry_id",
            )
            .bind(row.operation_id.to_string())
            .fetch_all(&mut *tx)
            .await
            .map_err(unavailable)?;
            for encoded in entries {
                entry_current(store, &snapshot, &current, &submitted, &decode(&encoded)?)?;
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(error) = authorized {
            match error {
                Error::NotFound | Error::Conflict => authorization_lost = true,
                other => return Err(other),
            }
        }
    }
    if !body.entries.is_empty() && !authorization_lost {
        if row.state != "live" || row.deadline.timestamp_millis() <= at || body.terminal {
            return Err(Error::Conflict);
        }
        let current = current_principal(&snapshot, row.api_key_id)?;
        accept_entries(
            store,
            &mut tx,
            &snapshot,
            &current,
            &submitted,
            &body.entries,
        )
        .await?;
    }
    let has_usage:i64=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_live_journal WHERE operation_id=? AND json_array_length(usage)>0)").bind(row.operation_id.to_string()).fetch_one(&mut *tx).await.map_err(unavailable)?;
    let lost = row.coverage == "unknown"
        || body.coverage == UsageCoverage::Unknown
        || row.deadline.timestamp_millis() <= at
        || row.state == "requires_stop"
        || authorization_lost
        || (body.terminal && has_usage == 0);
    let next_coverage = if lost {
        "unknown"
    } else {
        coverage(body.coverage)
    };
    let state = if body.terminal {
        "closed"
    } else if row.deadline.timestamp_millis() <= at || authorization_lost {
        "requires_stop"
    } else {
        row.state.as_str()
    };
    let expiry = if state == "requires_stop" {
        at + LIVE_LEASE_MS
    } else {
        (at + LIVE_LEASE_MS).min(row.deadline.timestamp_millis())
    };
    sqlx::query("UPDATE native_live_sessions SET state=?,coverage=?,accounting_state=CASE WHEN ?='unknown' OR (?='closed' AND ?<>'complete') THEN 'awaiting_reconciliation' ELSE accounting_state END,lease_expires_at=?,closed_at=CASE WHEN ?='closed' THEN ? ELSE NULL END WHERE operation_id=?")
        .bind(state).bind(next_coverage).bind(next_coverage).bind(state).bind(next_coverage).bind(expiry).bind(state).bind(at).bind(row.operation_id.to_string()).execute(&mut *tx).await.map_err(unavailable)?;
    let reply = live_reply(&live(&mut tx, row.operation_id).await?, None)?;
    tx.commit().await.map_err(unavailable)?;
    Ok(reply)
}

pub(super) async fn claim_live_sessions(
    store: &StandaloneNativeStore,
    body: ClaimLiveSessionsRequest,
) -> Result<NativeReply> {
    if body.limit == 0 || body.limit > 100 {
        return Err(Error::InvalidRequest);
    }
    let mut tx = store.write_tx().await?;
    let ids:Vec<String>=sqlx::query_scalar("SELECT operation_id FROM native_live_sessions WHERE state<>'closed' AND lease_expires_at<=? ORDER BY lease_expires_at,operation_id LIMIT ?").bind(now()).bind(body.limit as i64).fetch_all(&mut *tx).await.map_err(unavailable)?;
    let mut reply = NativeReply::new("claimed");
    for id in ids {
        let id = uuid(&id)?;
        sqlx::query("UPDATE native_live_sessions SET state='requires_stop',coverage='unknown',accounting_state='awaiting_reconciliation',lease_worker=?,lease_generation=lease_generation+1,lease_expires_at=? WHERE operation_id=?")
            .bind(store.worker_id.to_string()).bind(now()+LIVE_LEASE_MS).bind(id.to_string()).execute(&mut *tx).await.map_err(unavailable)?;
        let mut record = live_record(&live(&mut tx, id).await?)?;
        record.accepted = Some(operation(&mut tx, id).await?.0);
        reply.live_sessions.push(record);
    }
    tx.commit().await.map_err(unavailable)?;
    Ok(reply)
}

struct SipBinding {
    provider_id: Uuid,
    provider: std::sync::Arc<aisix_core::resource::ResourceEntry<ProviderKey>>,
    key_id: Uuid,
    model_id: Uuid,
    signing_key: Vec<u8>,
    namespace: (String, String),
}
fn sip_binding(snapshot: &AisixSnapshot, route: &str) -> Result<SipBinding> {
    let mut bindings = Vec::new();
    for provider in snapshot.provider_keys.entries() {
        let Some(config) = &provider.value.realtime_sip else {
            continue;
        };
        let signing_key = config.signing_key().map_err(|_| Error::NotFound)?;
        let namespace = provider
            .value
            .realtime_sip_namespace()
            .map_err(|_| Error::NotFound)?;
        let binding = SipBinding {
            provider_id: stable_resource_id("provider_keys", &provider.id),
            key_id: stable_resource_id("apikeys", &config.api_key_id),
            model_id: stable_resource_id("models", &config.model_id),
            signing_key,
            namespace,
            provider: provider.clone(),
        };
        bindings.push((config.route_id.clone(), binding));
    }
    let index = bindings
        .iter()
        .position(|(id, _)| id == route)
        .ok_or(Error::NotFound)?;
    let selected = &bindings[index].1;
    if bindings.iter().enumerate().any(|(other, (id, binding))| {
        other != index
            && (id == route
                || binding.signing_key == selected.signing_key
                || binding.namespace == selected.namespace)
    }) {
        return Err(Error::NotFound);
    }
    Ok(bindings.swap_remove(index).1)
}
fn sip_signature_valid(binding: &SipBinding, body: &SignedSipIntakeRequest) -> bool {
    let Ok(timestamp) = body.webhook_timestamp.parse::<i64>() else {
        return false;
    };
    if Utc::now().timestamp().abs_diff(timestamp) > 300 {
        return false;
    }
    let signed = format!(
        "{}.{}.{}",
        body.webhook_id, body.webhook_timestamp, body.raw_body
    );
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &binding.signing_key);
    // 官方 SDK 3dae6090 _webhooks.py 同时兼容 v1, 和裸 Base64；未知版本不会解码成功。
    body.webhook_signature.split_whitespace().any(|signature| {
        base64::engine::general_purpose::STANDARD
            .decode(signature.strip_prefix("v1,").unwrap_or(signature))
            .is_ok_and(|signature| ring::hmac::verify(&key, signed.as_bytes(), &signature).is_ok())
    })
}
fn sip_submission(
    store: &StandaloneNativeStore,
    snapshot: &AisixSnapshot,
    binding: &SipBinding,
    body: &SignedSipIntakeRequest,
    event: &str,
) -> Result<BeginRequest> {
    let mut models = snapshot
        .models
        .entries()
        .into_iter()
        .filter(|model| stable_resource_id("models", &model.id) == binding.model_id);
    let model = models.next().ok_or(Error::NotFound)?;
    if models.next().is_some()
        || model.value.provider_key_id.as_deref() != Some(binding.provider.id.as_str())
    {
        return Err(Error::NotFound);
    }
    let upstream = model
        .value
        .model_name
        .as_ref()
        .filter(|model| !model.is_empty())
        .ok_or(Error::NotFound)?;
    let surface = Some(native_origin_surface(
        NativeProtocol::Openai,
        NativeObjectKind::RealtimeCall,
    ));
    let boundary = configured_native_origin_at(
        &store.id_key,
        binding.provider_id,
        &binding.provider.value,
        NativeProtocol::Openai,
        NativeObjectKind::RealtimeCall,
        surface,
    )
    .map_err(|_| Error::NotFound)?;
    Ok(BeginRequest {
        operation_id: body.operation_id,
        api_key_id: binding.key_id,
        protocol: NativeProtocol::Openai,
        kind: NativeObjectKind::RealtimeCall,
        origin: OriginBinding {
            model_id: binding.model_id,
            provider_key_id: binding.provider_id,
            boundary,
            surface,
        },
        parent_id: None,
        input_file_id: None,
        requested_model_id: binding.model_id,
        requested_model: model.value.display_name.clone(),
        upstream_model: upstream.clone(),
        upstream_provider: model.value.provider.clone().unwrap_or_default(),
        operation: "realtime".into(),
        request_id: event.into(),
        asynchronous: false,
        requested_service_tier: None,
        entries: vec![],
        observation_headers: Default::default(),
        upstream_input_text: None,
    })
}

pub(super) async fn signed_sip_intake(
    store: &StandaloneNativeStore,
    body: SignedSipIntakeRequest,
) -> Result<NativeReply> {
    if !text_ok(&body.route_id, 64)
        || !body
            .route_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        || !text_ok(&body.webhook_id, 512)
        || body.raw_body.len() > 256 * 1024
        || body.webhook_signature.len() > 4096
    {
        return Err(Error::NotFound);
    }
    let mut tx = store.write_tx().await?;
    let snapshot = store.snapshot.load();
    let binding = sip_binding(&snapshot, &body.route_id)?;
    if !sip_signature_valid(&binding, &body) {
        return Err(Error::NotFound);
    }
    let signed: Value = serde_json::from_str(&body.raw_body).map_err(|_| Error::NotFound)?;
    if signed.get("type").and_then(Value::as_str) != Some("realtime.call.incoming") {
        return Err(Error::NotFound);
    }
    let event = signed
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| text_ok(id, 512))
        .ok_or(Error::NotFound)?;
    let call = signed
        .pointer("/data/call_id")
        .and_then(Value::as_str)
        .filter(|id| text_ok(id, 512))
        .ok_or(Error::NotFound)?;
    let current = current_principal(&snapshot, binding.key_id)?;
    let authoritative = sip_submission(store, &snapshot, &binding, &body, event)?;
    store.authorize_origin(
        &snapshot,
        Some(&current),
        &authoritative,
        NativeObjectKind::RealtimeCall,
        Some(EndpointFamily::Realtime),
    )?;
    // 先验证当前可信绑定并去重；拒绝和重复 delivery 都不会先创建孤立 operation。
    let old=sqlx::query("SELECT operation_id,route_id,provider_event_id,call_id,provider_key_id FROM native_sip_intakes WHERE boundary=? AND (provider_event_id=? OR call_id=? OR webhook_id=?)")
        .bind(authoritative.origin.boundary.as_str()).bind(event).bind(call).bind(&body.webhook_id).fetch_all(&mut *tx).await.map_err(unavailable)?;
    if let Some(first) = old.first() {
        if old.len() != 1
            || first.try_get::<&str, _>("route_id").map_err(unavailable)? != body.route_id
            || first
                .try_get::<&str, _>("provider_event_id")
                .map_err(unavailable)?
                != event
            || first.try_get::<&str, _>("call_id").map_err(unavailable)? != call
            || first
                .try_get::<&str, _>("provider_key_id")
                .map_err(unavailable)?
                != binding.provider_id.to_string()
        {
            return Err(Error::Conflict);
        }
        let id = uuid(first.try_get("operation_id").map_err(unavailable)?)?;
        let row = live(&mut tx, id).await?;
        if row.api_key_id != binding.key_id || row.owner != current.owner {
            return Err(Error::NotFound);
        }
        return live_reply(&row, Some(false));
    }
    let exists: i64 =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_operations WHERE operation_id=?)")
            .bind(body.operation_id.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?;
    if exists == 0 {
        // 实时 intake 不设置图扩展标志，统一采用私有协议声明的缺省值。
        let begin: BeginRelatedRequest =
            serde_json::from_value(serde_json::json!({"submission":authoritative}))
                .map_err(unavailable)?;
        store.begin_inner(&mut tx, begin).await?;
    }
    let (submitted, owner, worker) = operation(&mut tx, body.operation_id).await?;
    current_operation(store, &snapshot, &current, &submitted, &owner, worker)?;
    if submitted.origin.provider_key_id != binding.provider_id
        || submitted.origin.model_id != binding.model_id
        || submitted.requested_model_id != binding.model_id
        || submitted.origin.boundary != authoritative.origin.boundary
    {
        return Err(Error::NotFound);
    }
    let mut reply = create_inner(
        store,
        &mut tx,
        CreateLiveSessionRequest {
            operation_id: body.operation_id,
            api_key_id: binding.key_id,
            purpose: RealtimePurpose::Realtime,
            transport: RealtimeTransport::Sip,
            credential_id: None,
            provider_session_id: Some(call.into()),
            deadline_at: body.deadline_at,
        },
        true,
    )
    .await?;
    let graph_binding: BindRelatedRequest = serde_json::from_value(serde_json::json!({
        "binding":BindRequest {
            operation_id:body.operation_id,api_key_id:binding.key_id,raw_id:call.into(),
            existing_record_id:None,objects:vec![],provider_created_at:None,provider_expires_at:None,
        }
    })).map_err(unavailable)?;
    let bound = store.bind_inner(&mut tx, graph_binding).await?;
    sqlx::query("INSERT INTO native_sip_intakes(boundary,provider_event_id,webhook_id,call_id,route_id,provider_key_id,operation_id) VALUES(?,?,?,?,?,?,?)")
        .bind(authoritative.origin.boundary.as_str()).bind(event).bind(&body.webhook_id).bind(call).bind(&body.route_id).bind(binding.provider_id.to_string()).bind(body.operation_id.to_string()).execute(&mut *tx).await.map_err(unavailable)?;
    reply.record = bound.record;
    reply.appended = Some(true);
    tx.commit().await.map_err(unavailable)?;
    Ok(reply)
}
