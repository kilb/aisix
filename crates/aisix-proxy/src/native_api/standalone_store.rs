//! 单机原生对象归属：SQLite rollback/FULL 事务、稳定账号密钥和每次读取的当前授权。
//!
//! 多进程只能共享同一台机器上的同一个本地数据库文件；不支持网络文件系统或多机副本。
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use aisix_core::native_metering::{
    validate_observed_usage, MeteringTarget, ObservedMeter, ReconciliationRequest,
    TypedCompleteRequest,
};
use aisix_core::{
    models::EndpointFamily, native_objects::*, snapshot::SnapshotHandle, AisixSnapshot, ApiKey,
};
use chrono::{TimeZone, Utc};
use futures::future::BoxFuture;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    Row, SqliteConnection, SqlitePool,
};
use uuid::Uuid;

use super::registry::{NativeObjectRegistry, NativeRegistryError as Error};

pub(super) type Result<T> = std::result::Result<T, Error>;
const LEASE_MS: i64 = 60_000;
const MAX_ITEMS: usize = 50_000;

/// 只映射配置资源的内部标识；上游裸对象 ID 从不传给该函数。
pub fn stable_resource_id(kind: &str, raw: &str) -> Uuid {
    Uuid::parse_str(raw).unwrap_or_else(|_| {
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("aisix-standalone-native-v1\0{kind}\0{raw}").as_bytes(),
        )
    })
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Owner {
    pub(super) key_id: String,
    pub(super) team_id: Option<String>,
    pub(super) user_id: Option<String>,
}
impl Owner {
    fn permits(&self, caller: &Self) -> bool {
        self.team_id == caller.team_id
            && match &self.user_id {
                Some(user) => caller.user_id.as_ref() == Some(user),
                None => caller.user_id.is_none() && self.key_id == caller.key_id,
            }
    }
}

pub(super) struct Principal {
    pub(super) owner: Owner,
    pub(super) key: Arc<aisix_core::resource::ResourceEntry<ApiKey>>,
}

#[derive(Clone)]
struct Stored {
    record: NativeRecord,
    owner: Owner,
    submission: BeginRequest,
}

type StoredOperation = (Owner, BeginRelatedRequest, String, Option<String>);

// 新产物的调用者和遥测沿用已受理执行，不能随 owner 改成所属集合的创建者。
struct DerivedCapture {
    caller: String,
    telemetry: String,
}
impl DerivedCapture {
    fn for_entry(
        &self,
        templates: &BTreeMap<String, serde_json::Value>,
        entry_id: &str,
    ) -> Result<Self> {
        let template = templates.get(entry_id).ok_or(Error::Unavailable)?;
        Ok(Self {
            caller: self.caller.clone(),
            telemetry: json(&BTreeMap::from([("", template)]))?,
        })
    }
}
struct ObservedSource {
    related: BeginRelatedRequest,
    capture: Option<DerivedCapture>,
    templates: Option<BTreeMap<String, serde_json::Value>>,
}
#[derive(Clone, Copy)]
struct GraphReferences<'a> {
    parent_id: Option<Uuid>,
    references: &'a [AcceptedReference],
}

#[derive(Default)]
struct OwningChild<'a> {
    relations: Vec<&'a BoundRelation>,
    owner: Option<Owner>,
}
type OwningEdges<'a> = BTreeMap<(&'static str, &'a str), OwningChild<'a>>;
type ObjectKeys<'a> = BTreeSet<(&'static str, &'a str)>;

#[derive(Default)]
struct ChildOwners {
    records: BTreeMap<Uuid, Stored>,
    observed: BTreeMap<&'static str, BTreeMap<String, Owner>>,
}
struct DerivedGraph<'a> {
    root: &'a Stored,
    references: GraphReferences<'a>,
    objects: &'a ObjectKeys<'a>,
}

#[derive(Clone)]
pub struct StandaloneNativeStore {
    pub(super) pool: SqlitePool,
    pub(super) snapshot: SnapshotHandle<AisixSnapshot>,
    pub(super) id_key: [u8; 32],
    pub(super) worker_id: Uuid,
}

/// outbox 身份固定；接收方按 event_id 去重，重启重放不会生成第二个结算身份。
#[derive(Clone, Serialize, Deserialize)]
pub struct StandaloneTerminal {
    pub event_id: Uuid,
    pub delivery_generation: i64,
    pub canonical_event: String,
    pub record_id: Uuid,
    pub submission: BeginRequest,
    pub api_key_id: String,
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    pub usage: TerminalUsage,
}

fn unavailable<T>(_: T) -> Error {
    Error::Unavailable
}
fn json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(unavailable)
}
fn decode<T: DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(unavailable)
}
fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}
fn now() -> i64 {
    Utc::now().timestamp_millis()
}
fn account(a: &OriginBinding, b: &OriginBinding) -> bool {
    a.provider_key_id == b.provider_key_id && a.boundary == b.boundary
}

impl StandaloneNativeStore {
    /// 文件及其目录须已受保护；不自动创建丢失的目录、密钥或恢复归属。
    pub async fn open(
        path: &Path,
        id_key: [u8; 32],
        snapshot: SnapshotHandle<AisixSnapshot>,
    ) -> Result<Self> {
        protect_database(path)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false)
            .journal_mode(SqliteJournalMode::Delete)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .map_err(unavailable)?;
        let mut tx = pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(unavailable)?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?;
        if version > 1 {
            return Err(Error::Unavailable);
        }
        sqlx::raw_sql(include_str!("standalone_store.sql"))
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        let tag = json(&Sha256::digest(id_key).to_vec())?;
        sqlx::query("INSERT INTO native_meta(name,value) VALUES('origin_key_check',?) ON CONFLICT DO NOTHING")
            .bind(&tag).execute(&mut *tx).await.map_err(unavailable)?;
        let existing: String =
            sqlx::query_scalar("SELECT value FROM native_meta WHERE name='origin_key_check'")
                .fetch_one(&mut *tx)
                .await
                .map_err(unavailable)?;
        if existing != tag {
            return Err(Error::Conflict);
        }
        super::standalone_realtime_store::initialize(&mut tx).await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(Self {
            pool,
            snapshot,
            id_key,
            worker_id: Uuid::new_v4(),
        })
    }

    pub(super) async fn write_tx(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        self.pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(unavailable)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub(super) fn principal(snapshot: &AisixSnapshot, id: Uuid) -> Result<Principal> {
        let mut entries = snapshot
            .apikeys
            .entries()
            .into_iter()
            .filter(|entry| stable_resource_id("apikeys", &entry.id) == id);
        let key = entries.next().ok_or(Error::NotFound)?;
        if entries.next().is_some() || key.value.disabled || key.value.is_expired_at(Utc::now()) {
            return Err(Error::NotFound);
        }
        Ok(Principal {
            owner: Owner {
                key_id: key.id.clone(),
                team_id: key.value.team_id.clone(),
                user_id: key.value.user_id.clone(),
            },
            key,
        })
    }

    pub(super) fn authorize_origin(
        &self,
        snapshot: &AisixSnapshot,
        principal: Option<&Principal>,
        submitted: &BeginRequest,
        kind: NativeObjectKind,
        consumer: Option<EndpointFamily>,
    ) -> Result<()> {
        if consumer.is_some_and(|family| {
            family != kind.endpoint_family()
                && !native_object_consumer_allowed(submitted.protocol, kind, family)
        }) {
            return Err(Error::InvalidRequest);
        }
        let model = if principal.is_some() {
            Some(unique_resource(
                snapshot.models.entries(),
                "models",
                submitted.origin.model_id,
            )?)
        } else {
            None
        };
        let provider = unique_resource(
            snapshot.provider_keys.entries(),
            "provider_keys",
            submitted.origin.provider_key_id,
        )?;
        if model.as_ref().is_some_and(|model| {
            model.value.provider_key_id.as_deref() != Some(provider.id.as_str())
        }) {
            return Err(Error::NotFound);
        }
        let digest = configured_native_origin_at(
            &self.id_key,
            submitted.origin.provider_key_id,
            &provider.value,
            submitted.protocol,
            kind,
            submitted.origin.surface,
        )
        .map_err(|_| Error::NotFound)?;
        if digest != submitted.origin.boundary {
            return Err(Error::NotFound);
        }
        if let Some(principal) = principal {
            let requested = unique_resource(
                snapshot.models.entries(),
                "models",
                submitted.requested_model_id,
            )?;
            let requested_name = if requested.value.display_name.contains('*') {
                if !aisix_core::wildcard::wildcard_matches(
                    &requested.value.display_name,
                    &submitted.requested_model,
                ) {
                    return Err(Error::NotFound);
                }
                submitted.requested_model.as_str()
            } else {
                requested.value.display_name.as_str()
            };
            if !principal
                .key
                .value
                .can_call(consumer.unwrap_or_else(|| kind.endpoint_family()))
                || !principal.key.value.can_access(snapshot, requested_name)
                || (!principal.key.value.allowed_provider_groups.is_empty()
                    && !principal
                        .key
                        .value
                        .allowed_provider_groups
                        .iter()
                        .any(|g| provider.value.groups.contains(g)))
            {
                return Err(Error::NotFound);
            }
        }
        Ok(())
    }

    fn authorize(
        &self,
        snapshot: &AisixSnapshot,
        caller: &Principal,
        stored: &Stored,
        consumer: Option<EndpointFamily>,
    ) -> Result<()> {
        if stored.record.deleted || !stored.owner.permits(&caller.owner) {
            return Err(Error::NotFound);
        }
        self.authorize_origin(
            snapshot,
            Some(caller),
            &stored.submission,
            stored.record.kind.ok_or(Error::Unavailable)?,
            consumer,
        )?;
        // 同一账号和实际/请求模型范围只校验一次；不同请求别名仍各自保留权限边界。
        let mut checked = BTreeSet::from([(
            stored.submission.origin.provider_key_id,
            stored.submission.origin.model_id,
            stored.submission.requested_model_id,
            stored.submission.requested_model.as_str(),
        )]);
        let mut submitted = None;
        for entry in &stored.submission.entries {
            if !checked.insert((
                stored.submission.origin.provider_key_id,
                entry.model_id,
                entry.requested_model_id,
                entry.requested_model.as_str(),
            )) {
                continue;
            }
            let submitted = submitted.get_or_insert_with(|| stored.submission.clone());
            submitted.origin.model_id = entry.model_id;
            submitted.requested_model_id = entry.requested_model_id;
            submitted.requested_model.clone_from(&entry.requested_model);
            self.authorize_origin(
                snapshot,
                Some(caller),
                submitted,
                stored.submission.kind,
                consumer,
            )?;
        }
        Ok(())
    }

    async fn stored(conn: &mut SqliteConnection, id: Uuid) -> Result<Stored> {
        let row = sqlx::query("SELECT o.record,o.owner,p.submission,o.lease_worker,o.lease_until,o.lease_generation FROM native_objects o JOIN native_operations p USING(operation_id) WHERE o.record_id=?")
            .bind(id.to_string()).fetch_optional(&mut *conn).await.map_err(unavailable)?.ok_or(Error::NotFound)?;
        let mut record: NativeRecord = decode(row.try_get("record").map_err(unavailable)?)?;
        let worker: Option<String> = row.try_get("lease_worker").map_err(unavailable)?;
        let generation: i64 = row.try_get("lease_generation").map_err(unavailable)?;
        record.lease_generation = worker.map(|_| generation);
        record.lease_expires_at = row
            .try_get::<Option<i64>, _>("lease_until")
            .map_err(unavailable)?
            .and_then(|t| Utc.timestamp_millis_opt(t).single());
        Ok(Stored {
            record,
            owner: decode(row.try_get("owner").map_err(unavailable)?)?,
            submission: decode(row.try_get("submission").map_err(unavailable)?)?,
        })
    }

    async fn save_record(conn: &mut SqliteConnection, stored: &Stored) -> Result<()> {
        sqlx::query("UPDATE native_objects SET record=? WHERE record_id=?")
            .bind(json(&stored.record)?)
            .bind(stored.record.record_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn operation(
        conn: &mut SqliteConnection,
        id: Uuid,
    ) -> Result<(Owner, BeginRelatedRequest, String, Option<String>)> {
        let row = sqlx::query(
            "SELECT owner,related,state,root_id FROM native_operations WHERE operation_id=?",
        )
        .bind(id.to_string())
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?
        .ok_or(Error::NotFound)?;
        Ok((
            decode(row.try_get("owner").map_err(unavailable)?)?,
            decode(row.try_get("related").map_err(unavailable)?)?,
            row.try_get("state").map_err(unavailable)?,
            row.try_get("root_id").map_err(unavailable)?,
        ))
    }

    // 在生产者原有 operation 查询中同时取 immutable caller/telemetry，不按 child 另查。
    async fn operation_with_capture(
        conn: &mut SqliteConnection,
        id: Uuid,
        capture: bool,
    ) -> Result<(StoredOperation, Option<DerivedCapture>)> {
        if !capture {
            return Ok((Self::operation(conn, id).await?, None));
        }
        let row = sqlx::query("SELECT owner,related,state,root_id,caller,telemetry FROM native_operations WHERE operation_id=?")
            .bind(id.to_string()).fetch_optional(&mut *conn).await.map_err(unavailable)?.ok_or(Error::NotFound)?;
        Ok((
            (
                decode(row.try_get("owner").map_err(unavailable)?)?,
                decode(row.try_get("related").map_err(unavailable)?)?,
                row.try_get("state").map_err(unavailable)?,
                row.try_get("root_id").map_err(unavailable)?,
            ),
            Some(DerivedCapture {
                caller: row.try_get("caller").map_err(unavailable)?,
                telemetry: row.try_get("telemetry").map_err(unavailable)?,
            }),
        ))
    }

    fn accepted_telemetry(
        &self,
        snapshot: &AisixSnapshot,
        submission: &BeginRequest,
        caller: &Owner,
    ) -> Result<BTreeMap<String, serde_json::Value>> {
        let mut sources = vec![(String::new(), submission.clone())];
        for entry in &submission.entries {
            let mut source = submission.clone();
            source.origin.model_id = entry.model_id;
            source.requested_model_id = entry.requested_model_id;
            source.requested_model = entry.requested_model.clone();
            source.upstream_model = entry.upstream_model.clone();
            source.upstream_provider = entry.upstream_provider.clone();
            source.requested_service_tier = entry.requested_service_tier.clone();
            sources.push((entry.entry_id.clone(), source));
        }
        let mut templates = BTreeMap::new();
        for (id, source) in sources {
            let model =
                unique_resource(snapshot.models.entries(), "models", source.origin.model_id).ok();
            let requested = unique_resource(
                snapshot.models.entries(),
                "models",
                source.requested_model_id,
            )
            .ok();
            let provider = unique_resource(
                snapshot.provider_keys.entries(),
                "provider_keys",
                source.origin.provider_key_id,
            )
            .ok();
            // NO-GUARDRAIL-CHAIN: 只保存准入时身份，不生成用量或发出计费事件。
            let mut event = aisix_obs::UsageEvent {
                api_key_id: caller.key_id.clone(),
                team_id: caller.team_id.clone().unwrap_or_default(),
                user_id: caller.user_id.clone().unwrap_or_default(),
                model_id: model
                    .as_ref()
                    .map_or_else(|| source.origin.model_id.to_string(), |v| v.id.clone()),
                requested_model_id: requested
                    .as_ref()
                    .map_or_else(|| source.requested_model_id.to_string(), |v| v.id.clone()),
                requested_model: source.requested_model,
                upstream_model: Some(source.upstream_model),
                upstream_provider: Some(source.upstream_provider),
                provider_key_id: Some(provider.as_ref().map_or_else(
                    || source.origin.provider_key_id.to_string(),
                    |v| v.id.clone(),
                )),
                requested_service_tier: source.requested_service_tier,
                ..Default::default()
            };
            if let Some(key) = snapshot.apikeys.get_by_id(&caller.key_id) {
                let auth = crate::auth::AuthenticatedKey::new(snapshot, key, None, false);
                crate::usage_attr::apply_caller_identity(&mut event, None, &auth.caller());
            }
            if let Some(provider) = provider {
                crate::usage_attr::apply_pk_telemetry(
                    &mut event,
                    &crate::usage_attr::ResolvedPk::resolve(snapshot, &provider.id),
                );
            }
            crate::usage_attr::bound_outside_text(&mut event);
            event.requested_service_tier = event
                .requested_service_tier
                .map(crate::chat::sanitize_tag)
                .filter(|v| !v.is_empty());
            let wire = serde_json::to_value(event).map_err(unavailable)?;
            let fields = wire.as_object().ok_or(Error::Unavailable)?;
            let mut saved = serde_json::Map::new();
            for name in [
                "api_key_id",
                "api_key_name",
                "team_id",
                "user_id",
                "tags",
                "model_id",
                "requested_model_id",
                "requested_model",
                "upstream_model",
                "upstream_provider",
                "provider_key_id",
                "requested_service_tier",
                "provider_kind",
                "provider_featured",
                "branded_provider",
                "pk_label",
                "byo_label",
            ] {
                if let Some(value) = fields.get(name) {
                    saved.insert(name.into(), value.clone());
                }
            }
            if let Some(model) = model {
                if let Some(cost) = crate::usage_attr::pricing_index(snapshot).resolve(&model.value)
                {
                    saved.insert(
                        "_native_cost".into(),
                        serde_json::to_value(cost).map_err(unavailable)?,
                    );
                }
            }
            templates.insert(id, serde_json::Value::Object(saved));
        }
        Ok(templates)
    }

    pub(super) async fn begin_inner(
        &self,
        conn: &mut SqliteConnection,
        mut request: BeginRelatedRequest,
    ) -> Result<NativeReply> {
        request
            .references
            .sort_by_key(|reference| reference.record_id);
        if request
            .references
            .windows(2)
            .any(|pair| pair[0].record_id == pair[1].record_id)
        {
            return Err(Error::InvalidRequest);
        }
        let body = &request.submission;
        if request
            .lifecycle_context
            .as_ref()
            .is_some_and(|context| context.validate().is_err())
        {
            return Err(Error::InvalidRequest);
        }
        if body.entries.len() > MAX_ITEMS
            || request.references.len() > MAX_ITEMS
            || !valid_id(&body.request_id)
            || !valid_id(&body.operation)
        {
            return Err(Error::InvalidRequest);
        }
        let snapshot = self.snapshot.load();
        let principal = Self::principal(&snapshot, body.api_key_id)?;
        self.authorize_origin(&snapshot, Some(&principal), body, body.kind, None)?;
        let mut entry_ids = BTreeSet::new();
        for entry in &body.entries {
            if !entry_ids.insert(&entry.entry_id) || !valid_id(&entry.entry_id) {
                return Err(Error::InvalidRequest);
            }
            let mut submitted = body.clone();
            submitted.origin.model_id = entry.model_id;
            submitted.requested_model_id = entry.requested_model_id;
            submitted.requested_model = entry.requested_model.clone();
            self.authorize_origin(&snapshot, Some(&principal), &submitted, body.kind, None)?;
        }
        for (entry_id, references) in &mut request.entry_references {
            let entry = body
                .entries
                .iter()
                .find(|entry| entry.entry_id == *entry_id && entry.endpoint == "/v1/responses")
                .ok_or(Error::InvalidRequest)?;
            if references.len() > MAX_ITEMS {
                return Err(Error::InvalidRequest);
            }
            references.sort_by_key(|reference| reference.record_id);
            if references
                .windows(2)
                .any(|pair| pair[0].record_id == pair[1].record_id)
            {
                return Err(Error::InvalidRequest);
            }
            let mut submitted = body.clone();
            submitted.origin.model_id = entry.model_id;
            submitted.requested_model_id = entry.requested_model_id;
            submitted.requested_model = entry.requested_model.clone();
            for reference in references {
                let stored = Self::stored(conn, reference.record_id).await?;
                self.authorize_chain(
                    conn,
                    &snapshot,
                    &principal,
                    &stored,
                    Some(reference.consumer_family),
                )
                .await?;
                if !account(
                    stored.record.origin.as_ref().ok_or(Error::Unavailable)?,
                    &submitted.origin,
                ) {
                    return Err(Error::Conflict);
                }
            }
        }
        let mut owner = principal.owner.clone();
        if let Some(parent_id) = body.parent_id {
            let parent = Self::stored(conn, parent_id).await?;
            // File 自身仍按 Files 准入；合法 Batch 父上下文保留其 Batches 范围。
            let parent_consumer = if body.protocol == NativeProtocol::Openai
                && body.kind == NativeObjectKind::File
                && parent.submission.protocol == NativeProtocol::Openai
                && parent.submission.kind == NativeObjectKind::MessageBatch
            {
                EndpointFamily::Batches
            } else {
                body.kind.endpoint_family()
            };
            self.authorize_chain(conn, &snapshot, &principal, &parent, Some(parent_consumer))
                .await?;
            if !account(
                parent.record.origin.as_ref().ok_or(Error::Unavailable)?,
                &body.origin,
            ) {
                return Err(Error::NotFound);
            }
            if request.owner_parent_record_id == Some(parent_id) {
                owner = parent.owner;
            }
        }
        if let Some(owner_parent) = request.owner_parent_record_id {
            if Some(owner_parent) != body.parent_id
                && !(body.kind == NativeObjectKind::MessageContainer
                    && request
                        .references
                        .iter()
                        .any(|r| r.record_id == owner_parent))
            {
                return Err(Error::InvalidRequest);
            }
            let parent = Self::stored(conn, owner_parent).await?;
            if body.kind == NativeObjectKind::MessageContainer
                && parent.submission.kind != NativeObjectKind::MessageContainer
            {
                return Err(Error::InvalidRequest);
            }
            self.authorize_chain(
                conn,
                &snapshot,
                &principal,
                &parent,
                Some(body.kind.endpoint_family()),
            )
            .await?;
            if !account(
                parent.record.origin.as_ref().ok_or(Error::Unavailable)?,
                &body.origin,
            ) {
                return Err(Error::NotFound);
            }
            owner = parent.owner;
        }
        for reference in &request.references {
            let stored = Self::stored(conn, reference.record_id).await?;
            self.authorize_chain(
                conn,
                &snapshot,
                &principal,
                &stored,
                Some(reference.consumer_family),
            )
            .await?;
            if !account(
                stored.record.origin.as_ref().ok_or(Error::Unavailable)?,
                &body.origin,
            ) {
                return Err(Error::NotFound);
            }
        }
        if let Some(raw) = &body.input_file_id {
            let file = self
                .lookup_inner(
                    conn,
                    LookupRequest {
                        api_key_id: body.api_key_id,
                        consumer_family: Some(body.kind.endpoint_family()),
                        locator: ObjectLocator {
                            protocol: body.protocol,
                            kind: NativeObjectKind::File,
                            raw_id: raw.clone(),
                            origin: Some(body.origin.clone()),
                            parent_id: None,
                        },
                    },
                )
                .await?;
            if body
                .parent_id
                .is_some_and(|p| file.record.as_ref().is_none_or(|f| f.record_id != p))
            {
                return Err(Error::NotFound);
            }
        }
        let related = json(&request)?;
        sqlx::query("INSERT INTO native_operations(operation_id,owner,caller,submission,state,related,worker_id,telemetry) VALUES(?,?,?,?,'accepted',?,?,?) ON CONFLICT DO NOTHING")
            .bind(body.operation_id.to_string()).bind(json(&owner)?).bind(json(&principal.owner)?).bind(json(body)?).bind(&related).bind(self.worker_id.to_string()).bind(json(&self.accepted_telemetry(&snapshot, body, &principal.owner)?)?).execute(&mut *conn).await.map_err(unavailable)?;
        let (_, previous, state, _) = Self::operation(conn, body.operation_id).await?;
        if json(&previous)? != related {
            return Err(Error::Conflict);
        }
        if state == "accepted" {
            let mut parents = BTreeSet::new();
            parents.extend(body.parent_id);
            parents.extend(request.references.iter().map(|r| r.record_id));
            parents.extend(
                request
                    .entry_references
                    .values()
                    .flatten()
                    .map(|reference| reference.record_id),
            );
            for parent_id in parents {
                self.fence_free(conn, parent_id, Some(body.operation_id))
                    .await?;
                let parent = Self::stored(conn, parent_id).await?;
                sqlx::query("INSERT INTO native_operation_parents(operation_id,parent_id,version) VALUES(?,?,?) ON CONFLICT DO NOTHING")
                    .bind(body.operation_id.to_string()).bind(parent_id.to_string()).bind(parent.record.version).execute(&mut *conn).await.map_err(unavailable)?;
            }
        }
        let mut reply = NativeReply::new(if state == "accepted" {
            "pending_submission"
        } else {
            &state
        });
        reply.operation_id = Some(body.operation_id);
        reply.origin = Some(body.origin.clone());
        Ok(reply)
    }

    async fn insert_object(
        &self,
        conn: &mut SqliteConnection,
        submitted: BeginRequest,
        owner: Owner,
        raw_id: &str,
        derived: bool,
        capture: Option<&DerivedCapture>,
    ) -> Result<Stored> {
        if !valid_id(raw_id) {
            return Err(Error::InvalidRequest);
        }
        let existing: Option<String> = sqlx::query_scalar("SELECT record_id FROM native_objects WHERE protocol=? AND kind=? AND boundary=? AND raw_id=?")
            .bind(submitted.protocol.as_str()).bind(submitted.kind.as_str()).bind(submitted.origin.boundary.as_str()).bind(raw_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
        if let Some(id) = existing {
            let stored = Self::stored(conn, Uuid::parse_str(&id).map_err(unavailable)?).await?;
            if stored.record.deleted
                || !stored.owner.permits(&owner)
                || !account(
                    stored.record.origin.as_ref().ok_or(Error::Unavailable)?,
                    &submitted.origin,
                )
            {
                return Err(Error::Conflict);
            }
            if !derived && stored.record.operation_id != Some(submitted.operation_id) {
                return Err(Error::Conflict);
            }
            return Ok(stored);
        }
        if derived {
            let related = BeginRelatedRequest {
                entry_references: BTreeMap::new(),
                lifecycle_context: None,
                submission: submitted.clone(),
                references: vec![],
                owner_parent_record_id: None,
            };
            let fallback;
            let capture = if let Some(capture) = capture {
                capture
            } else {
                let caller: String = if let Some(parent) = submitted.parent_id {
                    sqlx::query_scalar("SELECT p.caller FROM native_objects o JOIN native_operations p USING(operation_id) WHERE o.record_id=?")
                        .bind(parent.to_string()).fetch_one(&mut *conn).await.map_err(unavailable)?
                } else {
                    json(&owner)?
                };
                fallback = DerivedCapture {
                    caller,
                    telemetry: json(&self.accepted_telemetry(
                        &self.snapshot.load(),
                        &submitted,
                        &owner,
                    )?)?,
                };
                &fallback
            };
            sqlx::query("INSERT INTO native_operations(operation_id,owner,caller,submission,state,related,worker_id,telemetry) VALUES(?,?,?,?,'bound',?,?,?)")
                .bind(submitted.operation_id.to_string()).bind(json(&owner)?).bind(&capture.caller).bind(json(&submitted)?).bind(json(&related)?).bind(self.worker_id.to_string()).bind(&capture.telemetry).execute(&mut *conn).await.map_err(unavailable)?;
        }
        let record = NativeRecord {
            upstream_input_text: submitted.upstream_input_text,
            input_file_id: submitted.input_file_id.clone(),
            record_id: Uuid::new_v4(),
            api_key_id: Some(stable_resource_id("apikeys", &owner.key_id)),
            entries: submitted.entries.clone(),
            observation_headers: submitted.observation_headers.clone(),
            completed: false,
            operation_id: Some(submitted.operation_id),
            requested_model_id: Some(submitted.requested_model_id),
            requested_model: submitted.requested_model.clone(),
            upstream_model: submitted.upstream_model.clone(),
            upstream_provider: submitted.upstream_provider.clone(),
            operation: submitted.operation.clone(),
            request_id: submitted.request_id.clone(),
            requested_service_tier: submitted.requested_service_tier.clone(),
            origin: Some(submitted.origin.clone()),
            protocol: Some(submitted.protocol),
            kind: Some(submitted.kind),
            raw_id: raw_id.to_owned(),
            parent_raw_id: if let Some(parent) = submitted.parent_id {
                Some(Self::stored(conn, parent).await?.record.raw_id)
            } else {
                None
            },
            parent_id: submitted.parent_id,
            lifecycle_context: None,
            metering_state: None,
            reconciliation_reason: None,
            version: 1,
            lease_generation: None,
            lease_expires_at: None,
            deleted: false,
        };
        sqlx::query("INSERT INTO native_objects(record_id,operation_id,protocol,kind,boundary,raw_id,owner,record,pending) VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(record.record_id.to_string()).bind(submitted.operation_id.to_string()).bind(submitted.protocol.as_str()).bind(submitted.kind.as_str()).bind(submitted.origin.boundary.as_str()).bind(raw_id).bind(json(&owner)?).bind(json(&record)?).bind(submitted.asynchronous).execute(&mut *conn).await.map_err(unavailable)?;
        sqlx::query("UPDATE native_operations SET state='bound',root_id=? WHERE operation_id=?")
            .bind(record.record_id.to_string())
            .bind(submitted.operation_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
        Ok(Stored {
            record,
            owner,
            submission: submitted,
        })
    }

    async fn lookup_inner(
        &self,
        conn: &mut SqliteConnection,
        request: LookupRequest,
    ) -> Result<NativeReply> {
        if !valid_id(&request.locator.raw_id) {
            return Err(Error::InvalidRequest);
        }
        let snapshot = self.snapshot.load();
        let principal = Self::principal(&snapshot, request.api_key_id)?;
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT record_id FROM native_objects WHERE protocol=? AND kind=? AND raw_id=?",
        )
        .bind(request.locator.protocol.as_str())
        .bind(request.locator.kind.as_str())
        .bind(&request.locator.raw_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(unavailable)?;
        let mut visible = Vec::new();
        for id in ids {
            let stored = Self::stored(conn, Uuid::parse_str(&id).map_err(unavailable)?).await?;
            if request
                .locator
                .origin
                .as_ref()
                .is_some_and(|o| stored.record.origin.as_ref().is_none_or(|s| !account(o, s)))
                || request
                    .locator
                    .parent_id
                    .is_some_and(|p| stored.record.parent_id != Some(p))
            {
                continue;
            }
            match self
                .authorize_chain(
                    conn,
                    &snapshot,
                    &principal,
                    &stored,
                    request.consumer_family,
                )
                .await
            {
                Ok(()) => visible.push(stored.record),
                Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        if visible.len() > 1 {
            return Err(Error::Conflict);
        }
        let mut reply = NativeReply::new("found");
        reply.record = Some(visible.pop().ok_or(Error::NotFound)?);
        Ok(reply)
    }

    pub(super) async fn bind_inner(
        &self,
        conn: &mut SqliteConnection,
        body: BindRelatedRequest,
    ) -> Result<NativeReply> {
        let binding_wire = json(&body)?;
        if body.binding.existing_record_id.is_none() {
            if let Some(row) =
                sqlx::query("SELECT request,reply FROM native_bindings WHERE operation_id=?")
                    .bind(body.binding.operation_id.to_string())
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(unavailable)?
            {
                if row.try_get::<&str, _>("request").map_err(unavailable)? != binding_wire {
                    return Err(Error::Conflict);
                }
                return decode(row.try_get("reply").map_err(unavailable)?);
            }
        }
        if body.binding.objects.len() > MAX_ITEMS || body.relations.len() > MAX_ITEMS {
            return Err(Error::InvalidRequest);
        }
        let ((owner, related, operation_state, root_id), capture) = Self::operation_with_capture(
            conn,
            body.binding.operation_id,
            !body.binding.objects.is_empty(),
        )
        .await?;
        if !matches!(operation_state.as_str(), "accepted" | "bound" | "uncertain") {
            return Err(Error::Conflict);
        }
        let mut owners = ChildOwners::default();
        let parent_rows = sqlx::query("SELECT parent_id,version FROM native_operation_parents WHERE operation_id=? AND active=1")
            .bind(body.binding.operation_id.to_string()).fetch_all(&mut *conn).await.map_err(unavailable)?;
        for row in parent_rows {
            let parent = Self::stored(
                conn,
                Uuid::parse_str(row.try_get("parent_id").map_err(unavailable)?)
                    .map_err(unavailable)?,
            )
            .await?;
            if parent.record.deleted
                || parent.record.version != row.try_get::<i64, _>("version").map_err(unavailable)?
            {
                return Err(Error::Conflict);
            }
            if capture.is_some()
                && matches!(
                    parent.submission.kind,
                    NativeObjectKind::Conversation | NativeObjectKind::ResponseContainer
                )
            {
                owners.records.insert(parent.record.record_id, parent);
            }
        }
        if related.submission.api_key_id != body.binding.api_key_id {
            return Err(Error::NotFound);
        }
        if let Some(existing) = body.binding.existing_record_id {
            if root_id.as_deref() != Some(existing.to_string().as_str()) {
                return Err(Error::NotFound);
            }
            let snapshot = self.snapshot.load();
            let principal = Self::principal(&snapshot, body.binding.api_key_id)?;
            let existing_record = Self::stored(conn, existing).await?;
            self.authorize_chain(conn, &snapshot, &principal, &existing_record, None)
                .await?;
        }
        let mut reused = None;
        if related.submission.kind == NativeObjectKind::MessageContainer {
            for reference in &related.references {
                let stored = Self::stored(conn, reference.record_id).await?;
                if stored.submission.kind == NativeObjectKind::MessageContainer
                    && stored.record.raw_id == body.binding.raw_id
                    && account(
                        stored.record.origin.as_ref().ok_or(Error::Unavailable)?,
                        &related.submission.origin,
                    )
                {
                    if stored.record.deleted || !stored.owner.permits(&owner) {
                        return Err(Error::Conflict);
                    }
                    reused = Some(stored);
                }
            }
        }
        let root = if let Some(stored) = reused {
            sqlx::query(
                "UPDATE native_operations SET state='bound',root_id=? WHERE operation_id=?",
            )
            .bind(stored.record.record_id.to_string())
            .bind(body.binding.operation_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
            stored
        } else {
            if let Some(existing) = root_id.as_deref() {
                let stored =
                    Self::stored(conn, Uuid::parse_str(existing).map_err(unavailable)?).await?;
                if stored.record.raw_id != body.binding.raw_id {
                    return Err(Error::Conflict);
                }
            }
            let mut stored = self
                .insert_object(
                    conn,
                    related.submission.clone(),
                    owner.clone(),
                    &body.binding.raw_id,
                    false,
                    None,
                )
                .await?;
            stored.record.lifecycle_context = related.lifecycle_context.clone();
            Self::save_record(conn, &stored).await?;
            stored
        };
        let references = GraphReferences {
            parent_id: related.submission.parent_id,
            references: &related.references,
        };
        let (mut owning_edges, observed_parents) = if body.binding.objects.is_empty() {
            (BTreeMap::new(), false)
        } else {
            Self::owning_edges(&body.relations)
        };
        let objects: ObjectKeys<'_> = if observed_parents {
            body.binding
                .objects
                .iter()
                .map(|object| (object.kind.as_str(), object.raw_id.as_str()))
                .collect()
        } else {
            BTreeSet::new()
        };
        let mut observed = Vec::new();
        for child in &body.binding.objects {
            let legacy_output = related.submission.kind == NativeObjectKind::Response
                && matches!(
                    child.kind,
                    NativeObjectKind::ResponseItem | NativeObjectKind::ResponseContainer
                );
            let related_output = body.relations.iter().any(|r| matches!(&r.child,ArtifactSelector::Observed { kind,raw_id } if *kind==child.kind && raw_id==&child.raw_id));
            if !legacy_output && !related_output {
                return Err(Error::InvalidRequest);
            }
            let mut derived = related.submission.clone();
            derived.operation_id = Uuid::new_v4();
            derived.kind = child.kind;
            derived.parent_id = (root.submission.kind == NativeObjectKind::Response
                && matches!(
                    child.kind,
                    NativeObjectKind::ResponseItem | NativeObjectKind::ResponseContainer
                ))
            .then_some(root.record.record_id);
            derived.asynchronous = false;
            let child_owner =
                match owning_edges.get_mut(&(child.kind.as_str(), child.raw_id.as_str())) {
                    Some(edges) => {
                        self.child_owner(
                            conn,
                            &DerivedGraph {
                                root: &root,
                                references,
                                objects: &objects,
                            },
                            child,
                            &mut owners,
                            edges,
                        )
                        .await?
                    }
                    None => None,
                }
                .unwrap_or_else(|| owner.clone());
            let stored = self
                .insert_object(
                    conn,
                    derived,
                    child_owner,
                    &child.raw_id,
                    true,
                    capture.as_ref(),
                )
                .await?;
            if observed_parents
                && matches!(
                    stored.submission.kind,
                    NativeObjectKind::Conversation | NativeObjectKind::ResponseContainer
                )
            {
                owners
                    .observed
                    .entry(stored.submission.kind.as_str())
                    .or_default()
                    .insert(stored.record.raw_id.clone(), stored.owner.clone());
            }
            if legacy_output {
                self.attach(
                    conn,
                    root.record.record_id,
                    stored.record.record_id,
                    NativeRelation::ResponseOutput,
                    body.binding.operation_id,
                )
                .await?;
            }
            observed.push(stored);
        }
        let mut reply = NativeReply::new("bound");
        reply.operation_id = Some(body.binding.operation_id);
        reply.record = Some(root.record.clone());
        reply.records = observed.iter().map(|s| s.record.clone()).collect();
        drop(owning_edges);
        for relation in body.relations {
            let parent = self
                .selector(conn, &root, &observed, references, relation.parent, &owners)
                .await?;
            let child = self
                .selector(conn, &root, &observed, references, relation.child, &owners)
                .await?;
            self.validate_relation(&parent, &child, relation.relation)?;
            self.fence_free(
                conn,
                parent.record.record_id,
                Some(body.binding.operation_id),
            )
            .await?;
            reply.memberships.push(
                self.attach(
                    conn,
                    parent.record.record_id,
                    child.record.record_id,
                    relation.relation,
                    body.binding.operation_id,
                )
                .await?,
            );
            let mut parent = Self::stored(conn, parent.record.record_id).await?;
            parent.record.version += 1;
            Self::save_record(conn, &parent).await?;
        }
        reply.record = Some(Self::stored(conn, root.record.record_id).await?.record);
        if body.binding.existing_record_id.is_none() {
            sqlx::query("INSERT INTO native_bindings(operation_id,request,reply) VALUES(?,?,?)")
                .bind(body.binding.operation_id.to_string())
                .bind(binding_wire)
                .bind(json(&reply)?)
                .execute(&mut *conn)
                .await
                .map_err(unavailable)?;
        }
        sqlx::query("UPDATE native_operation_parents SET active=0 WHERE operation_id=?")
            .bind(body.binding.operation_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
        Ok(reply)
    }

    fn owning_edges(relations: &[BoundRelation]) -> (OwningEdges<'_>, bool) {
        let mut indexed: OwningEdges<'_> = BTreeMap::new();
        let mut observed_parents = false;
        for relation in relations {
            if !matches!(
                relation.relation,
                NativeRelation::ConversationItem | NativeRelation::ContainerFile
            ) {
                continue;
            }
            if let ArtifactSelector::Observed { kind, raw_id } = &relation.child {
                observed_parents |= matches!(&relation.parent, ArtifactSelector::Observed { .. });
                indexed
                    .entry((kind.as_str(), raw_id.as_str()))
                    .or_default()
                    .relations
                    .push(relation);
            }
        }
        (indexed, observed_parents)
    }

    async fn selector<'a>(
        &self,
        conn: &mut SqliteConnection,
        root: &'a Stored,
        observed: &'a [Stored],
        references: GraphReferences<'_>,
        selector: ArtifactSelector,
        owners: &'a ChildOwners,
    ) -> Result<Cow<'a, Stored>> {
        match selector {
            ArtifactSelector::Root => Ok(Cow::Borrowed(root)),
            ArtifactSelector::Observed { kind, raw_id } => observed
                .iter()
                .find(|s| s.record.kind == Some(kind) && s.record.raw_id == raw_id)
                .map(Cow::Borrowed)
                .ok_or(Error::InvalidRequest),
            ArtifactSelector::Record { record_id } => {
                if references.parent_id != Some(record_id)
                    && !references
                        .references
                        .iter()
                        .any(|r| r.record_id == record_id)
                {
                    return Err(Error::InvalidRequest);
                }
                if let Some(parent) = owners.records.get(&record_id) {
                    return Ok(Cow::Borrowed(parent));
                }
                Self::stored(conn, record_id).await.map(Cow::Owned)
            }
        }
    }

    async fn child_owner(
        &self,
        conn: &mut SqliteConnection,
        graph: &DerivedGraph<'_>,
        child: &NativeChildObject,
        owners: &mut ChildOwners,
        edges: &mut OwningChild<'_>,
    ) -> Result<Option<Owner>> {
        if let Some(owner) = &edges.owner {
            return Ok(Some(owner.clone()));
        }
        if !matches!(
            child.kind,
            NativeObjectKind::ResponseItem | NativeObjectKind::ContainerFile
        ) {
            return Ok(None);
        }
        let mut owner: Option<Owner> = None;
        for relation in &edges.relations {
            let parent_kind = match relation.relation {
                NativeRelation::ConversationItem => NativeObjectKind::Conversation,
                NativeRelation::ContainerFile => NativeObjectKind::ResponseContainer,
                _ => continue,
            };
            let (kind, candidate) = match &relation.parent {
                ArtifactSelector::Root => (graph.root.submission.kind, graph.root.owner.clone()),
                ArtifactSelector::Record { record_id } => {
                    if graph.references.parent_id != Some(*record_id)
                        && !graph
                            .references
                            .references
                            .iter()
                            .any(|r| r.record_id == *record_id)
                    {
                        return Err(Error::InvalidRequest);
                    }
                    if let Some(parent) = owners.records.get(record_id) {
                        (parent.submission.kind, parent.owner.clone())
                    } else {
                        let parent = Self::stored(conn, *record_id).await?;
                        let candidate = (parent.submission.kind, parent.owner.clone());
                        owners.records.insert(*record_id, parent);
                        candidate
                    }
                }
                ArtifactSelector::Observed { kind, raw_id } => {
                    let candidate = if let Some(owner) = owners
                        .observed
                        .get(kind.as_str())
                        .and_then(|parents| parents.get(raw_id.as_str()))
                    {
                        owner.clone()
                    } else {
                        if !graph.objects.contains(&(kind.as_str(), raw_id.as_str())) {
                            return Err(Error::InvalidRequest);
                        }
                        // 反序父节点只按本次声明的精确账号身份查证一次；实际保存后以原owner覆盖。
                        let saved: Option<String> = sqlx::query_scalar("SELECT owner FROM native_objects WHERE protocol=? AND kind=? AND boundary=? AND raw_id=?")
                            .bind(graph.root.submission.protocol.as_str()).bind(kind.as_str())
                            .bind(&graph.root.submission.origin.boundary).bind(raw_id)
                            .fetch_optional(&mut *conn).await.map_err(unavailable)?;
                        let candidate = match saved {
                            Some(saved) => decode(&saved)?,
                            None => graph.root.owner.clone(),
                        };
                        owners
                            .observed
                            .entry(kind.as_str())
                            .or_default()
                            .insert(raw_id.clone(), candidate.clone());
                        candidate
                    };
                    (*kind, candidate)
                }
            };
            if kind != parent_kind
                || !relation
                    .relation
                    .permits(graph.root.submission.protocol, kind, child.kind)
            {
                return Err(Error::Conflict);
            }
            if owner.as_ref().is_some_and(|owner| owner != &candidate) {
                return Err(Error::Conflict);
            }
            owner = Some(candidate);
        }
        edges.owner = owner.clone();
        Ok(owner)
    }

    fn validate_relation(
        &self,
        parent: &Stored,
        child: &Stored,
        relation: NativeRelation,
    ) -> Result<()> {
        if parent.record.deleted
            || child.record.deleted
            || !parent.owner.permits(&child.owner)
            || !account(
                parent.record.origin.as_ref().ok_or(Error::Unavailable)?,
                child.record.origin.as_ref().ok_or(Error::Unavailable)?,
            )
            || !relation.permits(
                parent.submission.protocol,
                parent.submission.kind,
                child.submission.kind,
            )
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    async fn attach(
        &self,
        conn: &mut SqliteConnection,
        parent: Uuid,
        child: Uuid,
        relation: NativeRelation,
        operation: Uuid,
    ) -> Result<NativeMembership> {
        let existing = sqlx::query("SELECT membership_id,generation,version FROM native_memberships WHERE parent_id=? AND child_id=? AND relation=? AND deleted=0")
            .bind(parent.to_string()).bind(child.to_string()).bind(relation.as_str()).fetch_optional(&mut *conn).await.map_err(unavailable)?;
        if let Some(row) = existing {
            return Ok(NativeMembership {
                membership_id: Uuid::parse_str(row.try_get("membership_id").map_err(unavailable)?)
                    .map_err(unavailable)?,
                parent_record_id: parent,
                child_record_id: child,
                relation,
                generation: row.try_get("generation").map_err(unavailable)?,
                version: row.try_get("version").map_err(unavailable)?,
            });
        }
        let generation: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(generation),0)+1 FROM native_memberships WHERE parent_id=? AND child_id=? AND relation=?")
            .bind(parent.to_string()).bind(child.to_string()).bind(relation.as_str()).fetch_one(&mut *conn).await.map_err(unavailable)?;
        let membership = NativeMembership {
            membership_id: Uuid::new_v4(),
            parent_record_id: parent,
            child_record_id: child,
            relation,
            generation,
            version: 1,
        };
        sqlx::query("INSERT INTO native_memberships(membership_id,parent_id,child_id,relation,generation,operation_id) VALUES(?,?,?,?,?,?)")
            .bind(membership.membership_id.to_string()).bind(parent.to_string()).bind(child.to_string()).bind(relation.as_str()).bind(generation).bind(operation.to_string()).execute(&mut *conn).await.map_err(unavailable)?;
        Ok(membership)
    }

    async fn fence_free(
        &self,
        conn: &mut SqliteConnection,
        parent: Uuid,
        own: Option<Uuid>,
    ) -> Result<()> {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM native_reservations WHERE parent_id=? AND exclusive=1 AND state IN ('reserved','uncertain') AND (? IS NULL OR operation_id<>?)")
            .bind(parent.to_string()).bind(own.map(|v| v.to_string())).bind(own.map(|v| v.to_string())).fetch_one(&mut *conn).await.map_err(unavailable)?;
        if count != 0 {
            return Err(Error::Conflict);
        }
        for query in [
            "SELECT count(*) FROM native_operation_parents WHERE parent_id=? AND active=1 AND (? IS NULL OR operation_id<>?)",
            "SELECT count(*) FROM native_deletions WHERE parent_id=? AND state IN ('reserved','uncertain') AND (? IS NULL OR operation_id<>?)",
        ] {
            let count: i64 = sqlx::query_scalar(query).bind(parent.to_string()).bind(own.map(|v|v.to_string())).bind(own.map(|v|v.to_string())).fetch_one(&mut *conn).await.map_err(unavailable)?;
            if count != 0 { return Err(Error::Conflict); }
        }
        Ok(())
    }

    async fn reserve_inner(
        &self,
        conn: &mut SqliteConnection,
        body: ReserveRelationsRequest,
    ) -> Result<NativeReply> {
        if body.child_record_ids.len() > MAX_ITEMS {
            return Err(Error::InvalidRequest);
        }
        let snapshot = self.snapshot.load();
        let principal = Self::principal(&snapshot, body.api_key_id)?;
        let parent = Self::stored(conn, body.parent_record_id).await?;
        self.authorize_chain(conn, &snapshot, &principal, &parent, None)
            .await?;
        if parent.record.version != body.expected_parent_version {
            return Err(Error::Conflict);
        }
        self.fence_free(conn, body.parent_record_id, Some(body.operation_id))
            .await?;
        for reference in &body.references {
            let child = Self::stored(conn, reference.record_id).await?;
            self.authorize_chain(
                conn,
                &snapshot,
                &principal,
                &child,
                Some(reference.consumer_family),
            )
            .await?;
            if !account(
                parent.record.origin.as_ref().ok_or(Error::Unavailable)?,
                child.record.origin.as_ref().ok_or(Error::Unavailable)?,
            ) {
                return Err(Error::NotFound);
            }
        }
        for id in &body.child_record_ids {
            let child = Self::stored(conn, *id).await?;
            self.authorize_chain(
                conn,
                &snapshot,
                &principal,
                &child,
                Some(parent.submission.kind.endpoint_family()),
            )
            .await?;
            self.validate_relation(&parent, &child, body.relation)?;
            if body.action == RelationAction::Detach {
                let active: i64 = sqlx::query_scalar("SELECT count(*) FROM native_memberships WHERE parent_id=? AND child_id=? AND relation=? AND deleted=0")
                    .bind(body.parent_record_id.to_string()).bind(id.to_string()).bind(body.relation.as_str()).fetch_one(&mut *conn).await.map_err(unavailable)?;
                if active != 1 {
                    return Err(Error::NotFound);
                }
            }
        }
        let wire = json(&body)?;
        sqlx::query("INSERT INTO native_reservations(operation_id,request,parent_id,parent_version,owner,submission,state,exclusive) VALUES(?,?,?,?,?,?,'reserved',?) ON CONFLICT(operation_id) DO NOTHING")
            .bind(body.operation_id.to_string()).bind(&wire).bind(body.parent_record_id.to_string()).bind(body.expected_parent_version).bind(json(&parent.owner)?).bind(json(&parent.submission)?).bind(body.action != RelationAction::Discover).execute(&mut *conn).await.map_err(unavailable)?;
        let row = sqlx::query("SELECT request,state FROM native_reservations WHERE operation_id=?")
            .bind(body.operation_id.to_string())
            .fetch_one(&mut *conn)
            .await
            .map_err(unavailable)?;
        if row.get::<String, _>("request") != wire {
            return Err(Error::Conflict);
        }
        for reference in &body.references {
            let stored = Self::stored(conn, reference.record_id).await?;
            sqlx::query("INSERT INTO native_reservation_references(operation_id,record_id,version) VALUES(?,?,?) ON CONFLICT DO NOTHING")
                .bind(body.operation_id.to_string()).bind(reference.record_id.to_string()).bind(stored.record.version).execute(&mut *conn).await.map_err(unavailable)?;
        }
        let mut reply = NativeReply::new(row.get::<&str, _>("state"));
        reply.operation_id = Some(body.operation_id);
        reply.record = Some(parent.record);
        Ok(reply)
    }

    async fn commit_inner(
        &self,
        conn: &mut SqliteConnection,
        body: CommitRelationsRequest,
    ) -> Result<NativeReply> {
        if body.objects.len() > MAX_ITEMS {
            return Err(Error::InvalidRequest);
        }
        let row = sqlx::query("SELECT request,state,owner,submission,result FROM native_reservations WHERE operation_id=?").bind(body.operation_id.to_string()).fetch_optional(&mut *conn).await.map_err(unavailable)?.ok_or(Error::NotFound)?;
        let reservation: ReserveRelationsRequest =
            decode(row.try_get("request").map_err(unavailable)?)?;
        if reservation.api_key_id != body.api_key_id {
            return Err(Error::NotFound);
        }
        let state: &str = row.try_get("state").map_err(unavailable)?;
        if state == "committed" {
            let saved: (CommitRelationsRequest, NativeReply) =
                decode(row.try_get("result").map_err(unavailable)?)?;
            return if json(&saved.0)? == json(&body)? {
                Ok(saved.1)
            } else {
                Err(Error::Conflict)
            };
        }
        if !matches!(state, "reserved" | "uncertain") {
            return Err(Error::Conflict);
        }
        let references = sqlx::query(
            "SELECT record_id,version FROM native_reservation_references WHERE operation_id=?",
        )
        .bind(body.operation_id.to_string())
        .fetch_all(&mut *conn)
        .await
        .map_err(unavailable)?;
        for reference in references {
            let stored = Self::stored(
                conn,
                Uuid::parse_str(reference.try_get("record_id").map_err(unavailable)?)
                    .map_err(unavailable)?,
            )
            .await?;
            if stored.record.deleted
                || stored.record.version
                    != reference
                        .try_get::<i64, _>("version")
                        .map_err(unavailable)?
            {
                return Err(Error::Conflict);
            }
        }
        let parent = Self::stored(conn, reservation.parent_record_id).await?;
        if parent.record.deleted || parent.record.version != reservation.expected_parent_version {
            return Err(Error::Conflict);
        }
        self.fence_free(conn, reservation.parent_record_id, Some(body.operation_id))
            .await?;
        let owner: Owner = decode(row.try_get("owner").map_err(unavailable)?)?;
        let base: BeginRequest = decode(row.try_get("submission").map_err(unavailable)?)?;
        let mut children = Vec::new();
        for id in &reservation.child_record_ids {
            children.push(Self::stored(conn, *id).await?);
        }
        for object in &body.objects {
            if !reservation
                .relation
                .permits(base.protocol, base.kind, object.kind)
            {
                return Err(Error::InvalidRequest);
            }
            let existing: Option<String> = sqlx::query_scalar("SELECT record_id FROM native_objects WHERE protocol=? AND kind=? AND boundary=? AND raw_id=?")
                .bind(base.protocol.as_str()).bind(object.kind.as_str()).bind(base.origin.boundary.as_str()).bind(&object.raw_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
            let stored = if let Some(id) = existing {
                let id = Uuid::parse_str(&id).map_err(unavailable)?;
                if reservation.action != RelationAction::Discover
                    && !reservation.child_record_ids.contains(&id)
                {
                    return Err(Error::Conflict);
                }
                Self::stored(conn, id).await?
            } else {
                if reservation.action == RelationAction::Detach {
                    return Err(Error::NotFound);
                }
                let mut derived = base.clone();
                derived.operation_id = Uuid::new_v4();
                derived.kind = object.kind;
                derived.parent_id = None;
                derived.asynchronous = false;
                self.insert_object(conn, derived, owner.clone(), &object.raw_id, true, None)
                    .await?
            };
            if !children
                .iter()
                .any(|c| c.record.record_id == stored.record.record_id)
            {
                children.push(stored);
            }
        }
        let mut reply = NativeReply::new("committed");
        reply.operation_id = Some(body.operation_id);
        for child in children {
            self.validate_relation(&parent, &child, reservation.relation)?;
            if reservation.action == RelationAction::Detach {
                let removed=sqlx::query("UPDATE native_memberships SET deleted=1,version=version+1 WHERE parent_id=? AND child_id=? AND relation=? AND deleted=0 RETURNING membership_id,generation,version")
                    .bind(parent.record.record_id.to_string()).bind(child.record.record_id.to_string()).bind(reservation.relation.as_str()).fetch_optional(&mut *conn).await.map_err(unavailable)?.ok_or(Error::Conflict)?;
                reply.memberships.push(NativeMembership {
                    membership_id: Uuid::parse_str(
                        removed.try_get("membership_id").map_err(unavailable)?,
                    )
                    .map_err(unavailable)?,
                    parent_record_id: parent.record.record_id,
                    child_record_id: child.record.record_id,
                    relation: reservation.relation,
                    generation: removed.try_get("generation").map_err(unavailable)?,
                    version: removed.try_get("version").map_err(unavailable)?,
                });
            } else {
                if reservation.action == RelationAction::Discover {
                    let detached:i64=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM native_memberships WHERE parent_id=? AND child_id=? AND relation=? AND deleted=1) AND NOT EXISTS(SELECT 1 FROM native_memberships WHERE parent_id=? AND child_id=? AND relation=? AND deleted=0)")
                        .bind(parent.record.record_id.to_string()).bind(child.record.record_id.to_string()).bind(reservation.relation.as_str()).bind(parent.record.record_id.to_string()).bind(child.record.record_id.to_string()).bind(reservation.relation.as_str()).fetch_one(&mut *conn).await.map_err(unavailable)?;
                    if detached != 0 {
                        return Err(Error::Conflict);
                    }
                }
                reply.memberships.push(
                    self.attach(
                        conn,
                        parent.record.record_id,
                        child.record.record_id,
                        reservation.relation,
                        body.operation_id,
                    )
                    .await?,
                );
            }
            reply.records.push(child.record);
        }
        let mut parent = parent;
        parent.record.version += 1;
        Self::save_record(conn, &parent).await?;
        reply.record = Some(parent.record);
        sqlx::query(
            "UPDATE native_reservations SET state='committed',result=? WHERE operation_id=?",
        )
        .bind(json(&(body, reply.clone()))?)
        .bind(reservation.operation_id.to_string())
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
        Ok(reply)
    }

    async fn relation_lookup_inner(
        &self,
        conn: &mut SqliteConnection,
        body: LookupRelationsRequest,
    ) -> Result<NativeReply> {
        if body.raw_ids.len() > MAX_ITEMS {
            return Err(Error::InvalidRequest);
        }
        let snapshot = self.snapshot.load();
        let principal = Self::principal(&snapshot, body.api_key_id)?;
        let parent = Self::stored(conn, body.parent_record_id).await?;
        self.authorize_chain(conn, &snapshot, &principal, &parent, body.consumer_family)
            .await?;
        let rows = sqlx::query("SELECT membership_id,child_id,generation,version FROM native_memberships WHERE parent_id=? AND relation=? AND deleted=0 ORDER BY membership_id")
            .bind(body.parent_record_id.to_string()).bind(body.relation.as_str()).fetch_all(&mut *conn).await.map_err(unavailable)?;
        let mut reply = NativeReply::new("related");
        for row in rows {
            let child_id = Uuid::parse_str(row.try_get("child_id").map_err(unavailable)?)
                .map_err(unavailable)?;
            let child = Self::stored(conn, child_id).await?;
            if !body.raw_ids.is_empty() && !body.raw_ids.contains(&child.record.raw_id) {
                continue;
            }
            self.validate_relation(&parent, &child, body.relation)?;
            self.authorize_chain(
                conn,
                &snapshot,
                &principal,
                &child,
                body.consumer_family
                    .or(Some(parent.submission.kind.endpoint_family())),
            )
            .await?;
            reply.memberships.push(NativeMembership {
                membership_id: Uuid::parse_str(row.try_get("membership_id").map_err(unavailable)?)
                    .map_err(unavailable)?,
                parent_record_id: body.parent_record_id,
                child_record_id: child_id,
                relation: body.relation,
                generation: row.try_get("generation").map_err(unavailable)?,
                version: row.try_get("version").map_err(unavailable)?,
            });
            reply.records.push(child.record);
        }
        // 指定 ID 的关系查询逐个成立，不能靠同一父对象下另一个成员成功来授权。
        if !body.raw_ids.is_empty()
            && body
                .raw_ids
                .iter()
                .any(|id| !reply.records.iter().any(|r| &r.raw_id == id))
        {
            return Err(Error::NotFound);
        }
        Ok(reply)
    }

    async fn lease_record(
        &self,
        conn: &mut SqliteConnection,
        token: &LeaseToken,
    ) -> Result<Stored> {
        let row = sqlx::query("SELECT lease_worker,lease_generation,lease_until FROM native_objects WHERE record_id=?")
            .bind(token.record_id.to_string()).fetch_optional(&mut *conn).await.map_err(unavailable)?.ok_or(Error::NotFound)?;
        let stored = Self::stored(conn, token.record_id).await?;
        if row
            .try_get::<Option<String>, _>("lease_worker")
            .map_err(unavailable)?
            .as_deref()
            != Some(self.worker_id.to_string().as_str())
            || row
                .try_get::<i64, _>("lease_generation")
                .map_err(unavailable)?
                != token.generation
            || (!stored.record.completed
                && row
                    .try_get::<Option<i64>, _>("lease_until")
                    .map_err(unavailable)?
                    .is_none_or(|t| t <= now()))
        {
            return Err(Error::Conflict);
        }
        Ok(stored)
    }

    async fn complete_inner(
        &self,
        conn: &mut SqliteConnection,
        body: CompleteRequest,
    ) -> Result<NativeReply> {
        let stored = self.lease_record(conn, &body.lease).await?;
        let wire = json(&body)?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT terminal FROM native_objects WHERE record_id=?")
                .bind(body.lease.record_id.to_string())
                .fetch_one(&mut *conn)
                .await
                .map_err(unavailable)?;
        if let Some(previous) = existing {
            return if previous == wire {
                let mut reply = NativeReply::new("completed");
                reply.metering_state = stored.record.metering_state.clone();
                Ok(reply)
            } else {
                Err(Error::Conflict)
            };
        }
        let expected: BTreeSet<&str> = if stored.submission.entries.is_empty() {
            BTreeSet::from([""])
        } else {
            stored
                .submission
                .entries
                .iter()
                .map(|e| e.entry_id.as_str())
                .collect()
        };
        let observed: BTreeSet<&str> = body.usage.iter().map(|u| u.entry_id.as_str()).collect();
        let coverage = if stored.submission.kind == NativeObjectKind::EvalRun {
            !observed.is_empty() && observed.is_subset(&expected)
        } else {
            expected == observed
        };
        let status = match (stored.submission.protocol, stored.submission.kind) {
            (NativeProtocol::Openai, NativeObjectKind::EvalRun) => matches!(
                body.terminal_status.as_str(),
                "completed" | "failed" | "canceled"
            ),
            (NativeProtocol::Openai, _) => matches!(
                body.terminal_status.as_str(),
                "completed" | "failed" | "cancelled" | "incomplete" | "expired"
            ),
            (NativeProtocol::Anthropic, _) => body.terminal_status == "ended",
        };
        if !coverage || observed.len() != body.usage.len() || !status {
            return Err(Error::InvalidRequest);
        }
        if body.output_file_ids.len() > 2
            || (!body.output_file_ids.is_empty()
                && stored.submission.kind != NativeObjectKind::MessageBatch)
        {
            return Err(Error::InvalidRequest);
        }
        for raw in &body.output_file_ids {
            let mut derived = stored.submission.clone();
            derived.operation_id = Uuid::new_v4();
            derived.kind = NativeObjectKind::File;
            derived.parent_id = Some(stored.record.record_id);
            derived.input_file_id = None;
            derived.asynchronous = false;
            self.insert_object(conn, derived, stored.owner.clone(), raw, true, None)
                .await?;
        }
        let caller: String =
            sqlx::query_scalar("SELECT caller FROM native_operations WHERE operation_id=?")
                .bind(stored.submission.operation_id.to_string())
                .fetch_one(&mut *conn)
                .await
                .map_err(unavailable)?;
        let _: Owner = decode(&caller)?;
        let templates: BTreeMap<String, serde_json::Value> = decode(
            &sqlx::query_scalar::<_, String>(
                "SELECT telemetry FROM native_operations WHERE operation_id=?",
            )
            .bind(stored.submission.operation_id.to_string())
            .fetch_one(&mut *conn)
            .await
            .map_err(unavailable)?,
        )?;
        for usage in &body.usage {
            if usage.cached_prompt_tokens > usage.prompt_tokens
                || !(100..=599).contains(&usage.status_code)
                || usage.error_class.len() > 64
                || usage.error_message.len() > 4096
                || usage
                    .io_audit
                    .as_ref()
                    .is_some_and(|a| a.version != 2 || !a.validate())
            {
                return Err(Error::InvalidRequest);
            }
            let event_id = Uuid::new_v5(&stored.record.record_id, usage.entry_id.as_bytes());
            let template = templates.get(&usage.entry_id).ok_or(Error::Unavailable)?;
            let canonical_event =
                canonical_terminal_event(event_id, &stored.submission, usage, template)?;
            sqlx::query("INSERT INTO native_terminal_outbox(event_id,record_id,entry_id,submission,owner,usage,event) VALUES(?,?,?,?,?,?,?)")
                .bind(event_id.to_string()).bind(stored.record.record_id.to_string()).bind(&usage.entry_id).bind(json(&stored.submission)?).bind(&caller).bind(json(usage)?).bind(canonical_event).execute(&mut *conn).await.map_err(unavailable)?;
        }
        let mut stored = stored;
        stored.record.completed = true;
        stored.record.metering_state = Some("outbox_pending".into());
        stored.record.version += 1;
        Self::save_record(conn, &stored).await?;
        sqlx::query("UPDATE native_objects SET pending=0,terminal=? WHERE record_id=?")
            .bind(wire)
            .bind(stored.record.record_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
        let mut reply = NativeReply::new("completed");
        reply.metering_state = Some("outbox_pending".into());
        Ok(reply)
    }

    async fn observed_inner(
        &self,
        conn: &mut SqliteConnection,
        body: BindObservedArtifactsRequest,
    ) -> Result<NativeReply> {
        if body.entries.len() > MAX_ITEMS {
            return Err(Error::InvalidRequest);
        }
        let source = self.lease_record(conn, &body.lease).await?;
        let mut accepted_source: Option<ObservedSource> = None;
        let mut seen = BTreeSet::new();
        let mut reply = NativeReply::new("observed");
        for entry in body.entries {
            if !seen.insert(entry.entry_id.clone()) {
                return Err(Error::InvalidRequest);
            }
            let payload = json(&entry)?;
            let prior=sqlx::query("SELECT payload,reply FROM native_observed_entries WHERE record_id=? AND entry_id=?")
                .bind(source.record.record_id.to_string()).bind(&entry.entry_id).fetch_optional(&mut *conn).await.map_err(unavailable)?;
            if let Some(prior) = prior {
                if prior.try_get::<&str, _>("payload").map_err(unavailable)? != payload {
                    return Err(Error::Conflict);
                }
                let saved: NativeReply = decode(prior.try_get("reply").map_err(unavailable)?)?;
                reply.records.extend(saved.records);
                reply.memberships.extend(saved.memberships);
                continue;
            }
            let entry_id = entry.entry_id.clone();
            let records_start = reply.records.len();
            let memberships_start = reply.memberships.len();
            let mut submitted = source.submission.clone();
            if source.submission.entries.is_empty() {
                if !entry.entry_id.is_empty()
                    || entry.root.kind != source.submission.kind
                    || entry.root.raw_id != source.record.raw_id
                {
                    return Err(Error::InvalidRequest);
                }
            } else {
                let accepted = source
                    .submission
                    .entries
                    .iter()
                    .find(|e| e.entry_id == entry.entry_id)
                    .ok_or(Error::InvalidRequest)?;
                if accepted.endpoint != "/v1/responses"
                    || entry.root.kind != NativeObjectKind::Response
                {
                    return Err(Error::InvalidRequest);
                }
                submitted.origin.model_id = accepted.model_id;
                submitted.requested_model_id = accepted.requested_model_id;
                submitted.requested_model = accepted.requested_model.clone();
                submitted.upstream_model = accepted.upstream_model.clone();
                submitted.upstream_provider = accepted.upstream_provider.clone();
                submitted.requested_service_tier = accepted.requested_service_tier.clone();
                submitted.upstream_input_text = accepted.upstream_input_text;
                submitted.entries.clear();
                submitted.input_file_id = None;
            }
            if accepted_source.is_none() {
                let ((_, related, _, _), capture) = Self::operation_with_capture(
                    conn,
                    source.submission.operation_id,
                    !source.submission.entries.is_empty() || !entry.objects.is_empty(),
                )
                .await?;
                let templates = if source.submission.entries.is_empty() {
                    None
                } else {
                    Some(decode(
                        &capture.as_ref().ok_or(Error::Unavailable)?.telemetry,
                    )?)
                };
                accepted_source = Some(ObservedSource {
                    related,
                    capture,
                    templates,
                });
            }
            let accepted = accepted_source.as_ref().ok_or(Error::Unavailable)?;
            let entry_capture;
            let (references, capture) = if let Some(templates) = &accepted.templates {
                entry_capture = accepted
                    .capture
                    .as_ref()
                    .ok_or(Error::Unavailable)?
                    .for_entry(templates, &entry_id)?;
                (
                    GraphReferences {
                        parent_id: None,
                        references: accepted
                            .related
                            .entry_references
                            .get(&entry_id)
                            .map(Vec::as_slice)
                            .unwrap_or(&[]),
                    },
                    Some(&entry_capture),
                )
            } else {
                (
                    GraphReferences {
                        parent_id: accepted.related.submission.parent_id,
                        references: &accepted.related.references,
                    },
                    accepted.capture.as_ref(),
                )
            };
            let mut owners = ChildOwners::default();
            let root = if source.submission.entries.is_empty() {
                source.clone()
            } else {
                submitted.operation_id = Uuid::new_v5(
                    &source.record.record_id,
                    format!("response-entry\0{}", entry.entry_id).as_bytes(),
                );
                submitted.kind = NativeObjectKind::Response;
                submitted.parent_id = None;
                submitted.asynchronous = false;
                // 已存在 entry 的 root 必须幂等，不能让同一 entry 在重放时另认一个 root。
                let existing = Self::operation(conn, submitted.operation_id).await;
                if let Ok((_, _, _, Some(id))) = existing {
                    let stored =
                        Self::stored(conn, Uuid::parse_str(&id).map_err(unavailable)?).await?;
                    if stored.record.raw_id != entry.root.raw_id {
                        return Err(Error::Conflict);
                    }
                    stored
                } else {
                    self.insert_object(
                        conn,
                        submitted.clone(),
                        source.owner.clone(),
                        &entry.root.raw_id,
                        true,
                        capture,
                    )
                    .await?
                }
            };
            if !source.submission.entries.is_empty()
                && root.submission.operation_id != submitted.operation_id
            {
                return Err(Error::Conflict);
            }
            reply.records.push(root.record.clone());
            let (mut owning_edges, observed_parents) = if entry.objects.is_empty() {
                (BTreeMap::new(), false)
            } else {
                Self::owning_edges(&entry.relations)
            };
            let objects: ObjectKeys<'_> = if observed_parents {
                entry
                    .objects
                    .iter()
                    .map(|object| (object.kind.as_str(), object.raw_id.as_str()))
                    .collect()
            } else {
                BTreeSet::new()
            };
            let mut children = Vec::new();
            for object in &entry.objects {
                let legacy_output = root.submission.kind == NativeObjectKind::Response
                    && matches!(
                        object.kind,
                        NativeObjectKind::ResponseItem | NativeObjectKind::ResponseContainer
                    );
                let related_output=entry.relations.iter().any(|r| matches!(&r.child,ArtifactSelector::Observed{kind,raw_id} if *kind==object.kind && raw_id==&object.raw_id));
                if !legacy_output && !related_output {
                    return Err(Error::InvalidRequest);
                }
                let mut child = submitted.clone();
                child.operation_id = Uuid::new_v4();
                child.kind = object.kind;
                child.parent_id = (root.submission.kind == NativeObjectKind::Response
                    && matches!(
                        object.kind,
                        NativeObjectKind::ResponseItem | NativeObjectKind::ResponseContainer
                    ))
                .then_some(root.record.record_id);
                child.asynchronous = false;
                let child_owner =
                    match owning_edges.get_mut(&(object.kind.as_str(), object.raw_id.as_str())) {
                        Some(edges) => {
                            self.child_owner(
                                conn,
                                &DerivedGraph {
                                    root: &root,
                                    references,
                                    objects: &objects,
                                },
                                object,
                                &mut owners,
                                edges,
                            )
                            .await?
                        }
                        None => None,
                    }
                    .unwrap_or_else(|| source.owner.clone());
                let stored = self
                    .insert_object(conn, child, child_owner, &object.raw_id, true, capture)
                    .await?;
                if observed_parents
                    && matches!(
                        stored.submission.kind,
                        NativeObjectKind::Conversation | NativeObjectKind::ResponseContainer
                    )
                {
                    owners
                        .observed
                        .entry(stored.submission.kind.as_str())
                        .or_default()
                        .insert(stored.record.raw_id.clone(), stored.owner.clone());
                }
                if legacy_output {
                    reply.memberships.push(
                        self.attach(
                            conn,
                            root.record.record_id,
                            stored.record.record_id,
                            NativeRelation::ResponseOutput,
                            root.submission.operation_id,
                        )
                        .await?,
                    );
                }
                reply.records.push(stored.record.clone());
                children.push(stored);
            }
            drop(owning_edges);
            for relation in entry.relations {
                let parent = self
                    .selector(conn, &root, &children, references, relation.parent, &owners)
                    .await?;
                let child = self
                    .selector(conn, &root, &children, references, relation.child, &owners)
                    .await?;
                self.validate_relation(&parent, &child, relation.relation)?;
                self.fence_free(conn, parent.record.record_id, None).await?;
                reply.memberships.push(
                    self.attach(
                        conn,
                        parent.record.record_id,
                        child.record.record_id,
                        relation.relation,
                        root.submission.operation_id,
                    )
                    .await?,
                );
            }
            let mut saved = NativeReply::new("observed");
            saved.records = reply.records[records_start..].to_vec();
            saved.memberships = reply.memberships[memberships_start..].to_vec();
            sqlx::query("INSERT INTO native_observed_entries(record_id,entry_id,payload,reply) VALUES(?,?,?,?)")
                .bind(source.record.record_id.to_string()).bind(entry_id).bind(payload).bind(json(&saved)?).execute(&mut *conn).await.map_err(unavailable)?;
        }
        if let Some(complete) = body.complete {
            if complete.lease.record_id != body.lease.record_id
                || complete.lease.generation != body.lease.generation
            {
                return Err(Error::Conflict);
            }
            self.complete_inner(conn, complete).await?;
        }
        reply.record = Some(Self::stored(conn, source.record.record_id).await?.record);
        Ok(reply)
    }

    /// 读取尚未确认投递的持久终态；不需要原 caller 仍有效。
    pub async fn pending_terminals(&self, limit: u32) -> Result<Vec<StandaloneTerminal>> {
        if limit == 0 || limit > 100 {
            return Err(Error::InvalidRequest);
        }
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(unavailable)?;
        let rows = sqlx::query("SELECT event_id,record_id,submission,owner,usage,event,delivery_generation FROM native_terminal_outbox WHERE delivered=0 AND (delivery_until IS NULL OR delivery_until<=?) ORDER BY event_id LIMIT ?")
            .bind(now()).bind(limit).fetch_all(&mut *tx).await.map_err(unavailable)?;
        for row in &rows {
            sqlx::query("UPDATE native_terminal_outbox SET delivery_worker=?,delivery_until=?,delivery_generation=delivery_generation+1 WHERE event_id=?")
                .bind(self.worker_id.to_string()).bind(now()+LEASE_MS).bind(row.try_get::<&str,_>("event_id").map_err(unavailable)?).execute(&mut *tx).await.map_err(unavailable)?;
        }
        tx.commit().await.map_err(unavailable)?;
        rows.into_iter()
            .map(|r| {
                let owner: Owner = decode(r.try_get("owner").map_err(unavailable)?)?;
                Ok(StandaloneTerminal {
                    event_id: Uuid::parse_str(r.try_get("event_id").map_err(unavailable)?)
                        .map_err(unavailable)?,
                    delivery_generation: r
                        .try_get::<i64, _>("delivery_generation")
                        .map_err(unavailable)?
                        + 1,
                    canonical_event: r.try_get("event").map_err(unavailable)?,
                    record_id: Uuid::parse_str(r.try_get("record_id").map_err(unavailable)?)
                        .map_err(unavailable)?,
                    submission: decode(r.try_get("submission").map_err(unavailable)?)?,
                    api_key_id: owner.key_id,
                    team_id: owner.team_id,
                    user_id: owner.user_id,
                    usage: decode(r.try_get("usage").map_err(unavailable)?)?,
                })
            })
            .collect()
    }

    /// 只有仍拥有本轮投递租约的进程能够确认接收方已持久接收。
    pub async fn acknowledge_terminal_claim(&self, claim: &StandaloneTerminal) -> Result<()> {
        let updated = sqlx::query("UPDATE native_terminal_outbox SET delivered=1 WHERE event_id=? AND delivery_worker=? AND delivery_generation=? AND delivery_until>? AND delivered=0")
            .bind(claim.event_id.to_string()).bind(self.worker_id.to_string())
            .bind(claim.delivery_generation).bind(now()).execute(&self.pool).await.map_err(unavailable)?;
        if updated.rows_affected() != 1 {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    #[cfg(test)]
    async fn acknowledge_terminal(&self, event_id: Uuid) -> Result<()> {
        let row = sqlx::query("SELECT delivery_generation FROM native_terminal_outbox WHERE event_id=? AND delivery_worker=?")
            .bind(event_id.to_string()).bind(self.worker_id.to_string()).fetch_optional(&self.pool).await.map_err(unavailable)?.ok_or(Error::Conflict)?;
        let updated = sqlx::query("UPDATE native_terminal_outbox SET delivered=1 WHERE event_id=? AND delivery_worker=? AND delivery_generation=? AND delivery_until>? AND delivered=0")
            .bind(event_id.to_string()).bind(self.worker_id.to_string()).bind(row.try_get::<i64,_>("delivery_generation").map_err(unavailable)?)
            .bind(now()).execute(&self.pool).await.map_err(unavailable)?;
        if updated.rows_affected() != 1 {
            return Err(Error::Conflict);
        }
        Ok(())
    }
}

impl NativeObjectRegistry for StandaloneNativeStore {
    fn live_usage_authority(&self) -> super::registry::LiveUsageAuthority {
        super::registry::LiveUsageAuthority::AggregateSink
    }

    fn mint_credential(
        &self,
        request: aisix_core::native_realtime::MintCredentialRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(super::standalone_realtime_store::mint_credential(
            self, request,
        ))
    }
    fn redeem_credential(
        &self,
        request: aisix_core::native_realtime::RedeemCredentialRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(super::standalone_realtime_store::redeem_credential(
            self, request,
        ))
    }
    fn create_live_session(
        &self,
        request: aisix_core::native_realtime::CreateLiveSessionRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(super::standalone_realtime_store::create_live_session(
            self, request,
        ))
    }
    fn append_live_usage(
        &self,
        request: aisix_core::native_realtime::AppendLiveUsageRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(super::standalone_realtime_store::append_live_usage(
            self, request,
        ))
    }
    fn checkpoint_live_session(
        &self,
        request: aisix_core::native_realtime::CheckpointLiveSessionRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(super::standalone_realtime_store::checkpoint_live_session(
            self, request,
        ))
    }
    fn claim_live_sessions(
        &self,
        request: aisix_core::native_realtime::ClaimLiveSessionsRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(super::standalone_realtime_store::claim_live_sessions(
            self, request,
        ))
    }
    fn signed_sip_intake(
        &self,
        request: aisix_core::native_realtime::SignedSipIntakeRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(super::standalone_realtime_store::signed_sip_intake(
            self, request,
        ))
    }

    fn id_key(&self) -> Option<[u8; 32]> {
        Some(self.id_key)
    }
    fn resource_id(&self, kind: &str, raw: &str) -> Result<Uuid> {
        if raw.is_empty() {
            Err(Error::InvalidRequest)
        } else {
            Ok(stable_resource_id(kind, raw))
        }
    }
    fn begin(&self, request: BeginRequest) -> BoxFuture<'_, Result<NativeReply>> {
        self.begin_related(BeginRelatedRequest {
            entry_references: BTreeMap::new(),
            lifecycle_context: None,
            submission: request,
            references: vec![],
            owner_parent_record_id: None,
        })
    }
    fn begin_related(&self, request: BeginRelatedRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let reply = self.begin_inner(&mut tx, request).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn bind(&self, request: BindRequest) -> BoxFuture<'_, Result<NativeReply>> {
        self.bind_related(BindRelatedRequest {
            binding: request,
            relations: vec![],
        })
    }
    fn bind_related(&self, request: BindRelatedRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let reply = self.bind_inner(&mut tx, request).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn lookup(&self, request: LookupRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self.pool.begin().await.map_err(unavailable)?;
            let reply = self.lookup_inner(&mut tx, request).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn owned(&self, request: OwnedRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            if request.raw_ids.len() > MAX_ITEMS {
                return Err(Error::InvalidRequest);
            }
            let mut tx = self.pool.begin().await.map_err(unavailable)?;
            let snapshot = self.snapshot.load();
            Self::principal(&snapshot, request.api_key_id)?;
            let mut reply = NativeReply::new("owned");
            let mut seen = BTreeSet::new();
            for raw in request.raw_ids {
                if !seen.insert(raw.clone()) {
                    continue;
                }
                match self
                    .lookup_inner(
                        &mut tx,
                        LookupRequest {
                            api_key_id: request.api_key_id,
                            consumer_family: None,
                            locator: ObjectLocator {
                                protocol: request.protocol,
                                kind: request.kind,
                                raw_id: raw,
                                origin: Some(request.origin.clone()),
                                parent_id: request.parent_id,
                            },
                        },
                    )
                    .await
                {
                    Ok(found) => reply.records.extend(found.record),
                    Err(Error::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn tombstone(&self, request: TombstoneRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let snapshot = self.snapshot.load();
            let principal = Self::principal(&snapshot, request.api_key_id)?;
            let mut stored = Self::stored(&mut tx, request.record_id).await?;
            self.authorize_chain(&mut tx, &snapshot, &principal, &stored, None)
                .await?;
            if stored.record.version != request.version {
                return Err(Error::Conflict);
            }
            protect_ledger(&stored)?;
            self.fence_free(&mut tx, request.record_id, None).await?;
            stored.record.deleted = true;
            stored.record.version += 1;
            Self::save_record(&mut tx, &stored).await?;
            sqlx::query("UPDATE native_memberships SET deleted=1,version=version+1 WHERE parent_id=? AND deleted=0").bind(request.record_id.to_string()).execute(&mut *tx).await.map_err(unavailable)?;
            tx.commit().await.map_err(unavailable)?;
            Ok(NativeReply::new("deleted"))
        })
    }
    fn claim(&self, request: ClaimRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            if request.limit == 0 || request.limit > 50 {
                return Err(Error::InvalidRequest);
            }
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let ids: Vec<String> = sqlx::query_scalar("SELECT record_id FROM native_objects WHERE pending=1 AND poll_after<=? AND (lease_until IS NULL OR lease_until<=?) AND (? IS NULL OR record_id=?) AND (? IS NULL OR protocol=?) AND (? IS NULL OR kind=?) ORDER BY poll_after,record_id LIMIT ?")
                .bind(now()).bind(now()).bind(request.record_id.map(|i| i.to_string())).bind(request.record_id.map(|i| i.to_string())).bind(request.protocol.map(|p| p.as_str())).bind(request.protocol.map(|p| p.as_str())).bind(request.kind.map(|k| k.as_str())).bind(request.kind.map(|k| k.as_str())).bind(request.limit).fetch_all(&mut *tx).await.map_err(unavailable)?;
            let mut reply = NativeReply::new("leased");
            let snapshot = self.snapshot.load();
            for id in ids {
                let stored =
                    Self::stored(&mut tx, Uuid::parse_str(&id).map_err(unavailable)?).await?;
                if self
                    .authorize_origin(
                        &snapshot,
                        None,
                        &stored.submission,
                        stored.submission.kind,
                        None,
                    )
                    .is_err()
                {
                    sqlx::query("UPDATE native_objects SET poll_after=? WHERE record_id=?")
                        .bind(now() + 60_000)
                        .bind(&id)
                        .execute(&mut *tx)
                        .await
                        .map_err(unavailable)?;
                    continue;
                }
                sqlx::query("UPDATE native_objects SET lease_worker=?,lease_until=?,lease_generation=lease_generation+1 WHERE record_id=?")
                    .bind(self.worker_id.to_string()).bind(now()+LEASE_MS).bind(&id).execute(&mut *tx).await.map_err(unavailable)?;
                reply
                    .records
                    .push(Self::stored(&mut tx, stored.record.record_id).await?.record);
            }
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn renew(&self, request: LeaseRequest) -> BoxFuture<'_, Result<NativeReply>> {
        self.update_lease(request, true)
    }
    fn reschedule(&self, request: LeaseRequest) -> BoxFuture<'_, Result<NativeReply>> {
        self.update_lease(request, false)
    }
    fn reserve_delete(&self, request: ObjectDeletionRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let reply = self.delete_inner(&mut tx, request, false).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn commit_delete(&self, request: ObjectDeletionRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let reply = self.delete_inner(&mut tx, request, true).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn complete_typed(&self, request: TypedCompleteRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let reply = self.typed_inner(&mut tx, request).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn mark_reconciliation(
        &self,
        request: ReconciliationRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            if request.observed_usage.len() > MAX_ITEMS
                || request.raw_usage.as_ref().is_some_and(|raw| {
                    serde_json::to_vec(raw).map_or(true, |wire| wire.len() > 1024 * 1024)
                })
            {
                return Err(Error::InvalidRequest);
            }
            let (id, stored) = self.metering_target(&mut tx, &request.target).await?;
            if let MeteringTarget::Operation { operation_id, .. } = &request.target {
                let (_, related, state, root) = Self::operation(&mut tx, *operation_id).await?;
                if related.submission.asynchronous
                    && (request.reason
                        != aisix_core::native_metering::ReconciliationReason::UncertainSubmission
                        || root.is_some()
                        || state == "bound")
                {
                    return Err(Error::InvalidRequest);
                }
            }
            if let (MeteringTarget::Object { lease }, Some(_)) = (&request.target, &stored) {
                if !request.artifacts.is_empty() {
                    self.observed_inner(
                        &mut tx,
                        BindObservedArtifactsRequest {
                            lease: lease.clone(),
                            entries: request.artifacts.clone(),
                            complete: None,
                        },
                    )
                    .await?;
                }
            } else if !request.artifacts.is_empty() {
                return Err(Error::InvalidRequest);
            }
            let reply = self
                .reconciliation_receipt(&mut tx, id, stored.is_some(), &request)
                .await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn complete(&self, request: CompleteRequest) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let reply = self.complete_inner(&mut tx, request).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn reserve_relations(
        &self,
        request: ReserveRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let reply = self.reserve_inner(&mut tx, request).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn commit_relations(
        &self,
        request: CommitRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let reply = self.commit_inner(&mut tx, request).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn lookup_relations(
        &self,
        request: LookupRelationsRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self.pool.begin().await.map_err(unavailable)?;
            let reply = self.relation_lookup_inner(&mut tx, request).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
    fn resolve_reservation(
        &self,
        request: ResolveReservationRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let state = if request.upstream_rejected {
                "rejected"
            } else {
                "uncertain"
            };
            let mut matched = false;
            for (table, is_delete) in [("native_reservations", false), ("native_deletions", true)] {
                let row = sqlx::query(&format!(
                    "SELECT request,state FROM {table} WHERE operation_id=?"
                ))
                .bind(request.operation_id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(unavailable)?;
                if let Some(row) = row {
                    let caller = if is_delete {
                        decode::<ObjectDeletionRequest>(
                            row.try_get("request").map_err(unavailable)?,
                        )?
                        .api_key_id
                    } else {
                        decode::<ReserveRelationsRequest>(
                            row.try_get("request").map_err(unavailable)?,
                        )?
                        .api_key_id
                    };
                    if caller != request.api_key_id {
                        return Err(Error::NotFound);
                    }
                    let previous: &str = row.try_get("state").map_err(unavailable)?;
                    if previous == "committed" {
                        return Ok(NativeReply::new("committed"));
                    }
                    if previous == "rejected" && state != "rejected" {
                        return Err(Error::Conflict);
                    }
                    sqlx::query(&format!("UPDATE {table} SET state=? WHERE operation_id=?"))
                        .bind(state)
                        .bind(request.operation_id.to_string())
                        .execute(&mut *tx)
                        .await
                        .map_err(unavailable)?;
                    matched = true;
                    break;
                }
            }
            if !matched {
                let (_, related, previous, _) =
                    Self::operation(&mut tx, request.operation_id).await?;
                if related.submission.api_key_id != request.api_key_id {
                    return Err(Error::NotFound);
                }
                if previous == "bound" {
                    return Ok(NativeReply::new("bound"));
                }
                if previous == "rejected" && state != "rejected" {
                    return Err(Error::Conflict);
                }
                sqlx::query("UPDATE native_operations SET state=? WHERE operation_id=?")
                    .bind(state)
                    .bind(request.operation_id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(unavailable)?;
            }
            if request.upstream_rejected {
                sqlx::query("UPDATE native_operation_parents SET active=0 WHERE operation_id=?")
                    .bind(request.operation_id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(unavailable)?;
            }
            tx.commit().await.map_err(unavailable)?;
            Ok(NativeReply::new(state))
        })
    }
    fn bind_observed_artifacts(
        &self,
        request: BindObservedArtifactsRequest,
    ) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let reply = self.observed_inner(&mut tx, request).await?;
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
}

impl StandaloneNativeStore {
    fn update_lease(&self, body: LeaseRequest, renew: bool) -> BoxFuture<'_, Result<NativeReply>> {
        Box::pin(async move {
            if body.poll_after_ms > 3_600_000 {
                return Err(Error::InvalidRequest);
            }
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(unavailable)?;
            let stored = self.lease_record(&mut tx, &body.lease).await?;
            if stored.record.completed {
                return Ok(NativeReply::new("completed"));
            }
            if renew {
                sqlx::query("UPDATE native_objects SET lease_until=? WHERE record_id=?")
                    .bind(now() + LEASE_MS)
                    .bind(body.lease.record_id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(unavailable)?;
            } else {
                sqlx::query("UPDATE native_objects SET poll_after=?,lease_worker=NULL,lease_until=NULL WHERE record_id=?").bind(now()+i64::from(body.poll_after_ms)).bind(body.lease.record_id.to_string()).execute(&mut *tx).await.map_err(unavailable)?;
            }
            let mut reply = NativeReply::new(if renew { "leased" } else { "pending" });
            reply.record = Some(Self::stored(&mut tx, body.lease.record_id).await?.record);
            tx.commit().await.map_err(unavailable)?;
            Ok(reply)
        })
    }
}

fn protect_database(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(Error::InvalidRequest);
    }
    let parent = path.parent().ok_or(Error::InvalidRequest)?;
    if parent.canonicalize().map_err(unavailable)? != parent {
        return Err(Error::InvalidRequest);
    }
    let metadata = std::fs::metadata(parent).map_err(unavailable)?;
    if !metadata.is_dir() {
        return Err(Error::InvalidRequest);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidRequest);
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 => {
                return Err(Error::InvalidRequest)
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                {
                    Ok(file) => file.sync_all().map_err(unavailable)?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(unavailable(error)),
                }
                // 文件名和其目录一起持久化，避免新库刚创建时断电丢失目录项。
                std::fs::File::open(parent)
                    .and_then(|dir| dir.sync_all())
                    .map_err(unavailable)?;
            }
            Err(error) => return Err(unavailable(error)),
        }
    }
    #[cfg(not(unix))]
    {
        return Err(Error::Unavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path).map_err(unavailable)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidRequest);
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "standalone_store_tests.rs"]
mod tests;

/// 原样重放事务内保存的事件；只有接收方确认持久接收后才确认本轮租约。
pub async fn run_outbox(
    store: Option<Arc<StandaloneNativeStore>>,
    state: crate::state::ProxyState,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    let Some(store) = store else {
        return;
    };
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = cancel.changed() => { if changed.is_err() || *cancel.borrow() { return; } },
            _ = interval.tick() => {
                if *cancel.borrow() { return; }
                match store.pending_terminals(16).await {
                    Ok(entries) => {
                        let confirmations = futures::future::join_all(entries.into_iter().map(|entry| {
                            let store = &store;
                            let sink = &state.usage_sink;
                            async move {
                                let event=aisix_obs::usage::DurableUsageEvent::from_saved_json(&entry.event_id.to_string(), entry.canonical_event.clone());
                                let Ok(event)=event else { tracing::warn!("standalone native terminal payload is unavailable");return; };
                                if sink.emit_durable(event).await.is_ok() && store.acknowledge_terminal_claim(&entry).await.is_err() {
                                    tracing::warn!("standalone native terminal acknowledgement unavailable");
                                }
                            }
                        }));
                        tokio::select! {
                            changed=cancel.changed()=>{if changed.is_err() || *cancel.borrow(){return;}},
                            _=confirmations=>{},
                        }
                    },
                    Err(_) => tracing::warn!("standalone native terminal outbox unavailable"),
                }
            }
        }
    }
}

fn canonical_terminal_event(
    event_id: Uuid,
    source: &BeginRequest,
    usage: &TerminalUsage,
    template: &serde_json::Value,
) -> Result<String> {
    let entry = source.entries.iter().find(|e| e.entry_id == usage.entry_id);
    // NO-GUARDRAIL-CHAIN: 后台终态仅结算原准入；不重新扫描或重新派发模型。
    let event = aisix_obs::UsageEvent {
        event_id: event_id.to_string(),
        request_id: source.request_id.clone(),
        occurred_at: usage
            .occurred_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        io_audit: usage.io_audit.clone(),
        inference_stage: Some(InferenceStage::Terminal),
        native_operation_id: Some(source.operation_id),
        inbound_protocol: source.protocol.as_str().into(),
        status_code: usage.status_code,
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        cached_prompt_tokens: usage.cached_prompt_tokens,
        cache_usage_reported: usage.cache_usage_reported,
        cache_write_tokens: usage.cache_write_tokens,
        cache_creation_tokens: usage.cache_creation_tokens,
        cache_creation_ttl: aisix_gateway::CacheCreationTtl {
            cache_creation_5m_tokens: usage.cache_creation_5m_tokens,
            cache_creation_1h_tokens: usage.cache_creation_1h_tokens,
        },
        cache_read_tokens: usage.cache_read_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        total_tokens: usage.total_tokens,
        modality_tokens: aisix_gateway::ModalityTokens {
            prompt_audio_tokens: usage.prompt_audio_tokens,
            prompt_image_tokens: usage.prompt_image_tokens,
            completion_audio_tokens: usage.completion_audio_tokens,
            completion_image_tokens: usage.completion_image_tokens,
        },
        web_search_requests: usage.web_search_requests,
        usage_estimated: usage.usage_estimated,
        service_tier: usage.service_tier.clone(),
        requested_service_tier: entry
            .and_then(|e| e.requested_service_tier.clone())
            .or_else(|| source.requested_service_tier.clone()),
        error_class: usage.error_class.clone(),
        error_message: usage.error_message.clone(),
        is_batch: source.kind == NativeObjectKind::MessageBatch,
        ..Default::default()
    };
    let surface = match source.kind {
        NativeObjectKind::MessageBatch => crate::operation::BATCH_COMPLETION,
        NativeObjectKind::EvalRun => crate::operation::Surface {
            handler: "evals",
            operation: "eval_completion",
        },
        _ => crate::operation::RESPONSES,
    };
    let mut event = crate::usage_attr::prepare_usage_event_fields(surface, event, true, true);
    event.cache_creation_ttl = event.settled_cache_creation_ttl();
    if let Some(cost) = template.get("_native_cost") {
        let cost: aisix_core::models::ModelCost =
            serde_json::from_value(cost.clone()).map_err(unavailable)?;
        event.cost_usd = cost.calculate_with_cache(
            crate::usage_attr::input_tokens_from_counts(
                usage.prompt_tokens,
                usage.cached_prompt_tokens,
                usage.cache_write_tokens,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                event.cache_creation_ttl,
            ),
            u64::from(usage.completion_tokens),
        );
    }
    let mut wire = serde_json::to_value(&event).map_err(unavailable)?;
    wire.as_object_mut().ok_or(Error::Unavailable)?.extend(
        template
            .as_object()
            .ok_or(Error::Unavailable)?
            .iter()
            .filter(|(key, _)| key.as_str() != "_native_cost")
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    let canonical = json(&wire)?;
    aisix_obs::usage::DurableUsageEvent::from_saved_json(&event_id.to_string(), canonical.clone())
        .map_err(unavailable)?;
    Ok(canonical)
}

impl StandaloneNativeStore {
    async fn delete_inner(
        &self,
        conn: &mut SqliteConnection,
        request: ObjectDeletionRequest,
        commit: bool,
    ) -> Result<NativeReply> {
        let wire = json(&request)?;
        if !commit {
            let snapshot = self.snapshot.load();
            let principal = Self::principal(&snapshot, request.api_key_id)?;
            let stored = Self::stored(conn, request.record_id).await?;
            self.authorize_chain(conn, &snapshot, &principal, &stored, None)
                .await?;
            if stored.record.version != request.version {
                return Err(Error::Conflict);
            }
            protect_ledger(&stored)?;
            self.fence_free(conn, request.record_id, Some(request.operation_id))
                .await?;
            sqlx::query("INSERT INTO native_deletions(operation_id,request,parent_id,state) VALUES(?,?,?,'reserved') ON CONFLICT(operation_id) DO NOTHING")
                .bind(request.operation_id.to_string()).bind(&wire).bind(request.record_id.to_string()).execute(&mut *conn).await.map_err(unavailable)?;
        }
        let row = sqlx::query("SELECT request,state FROM native_deletions WHERE operation_id=?")
            .bind(request.operation_id.to_string())
            .fetch_optional(&mut *conn)
            .await
            .map_err(unavailable)?
            .ok_or(Error::NotFound)?;
        if row.try_get::<String, _>("request").map_err(unavailable)? != wire {
            return Err(Error::Conflict);
        }
        let state: &str = row.try_get("state").map_err(unavailable)?;
        if !commit {
            let mut reply = NativeReply::new(state);
            reply.operation_id = Some(request.operation_id);
            reply.record = Some(Self::stored(conn, request.record_id).await?.record);
            return Ok(reply);
        }
        if state == "committed" {
            return Ok(NativeReply::new("deleted"));
        }
        if !matches!(state, "reserved" | "uncertain") {
            return Err(Error::Conflict);
        }
        let mut stored = Self::stored(conn, request.record_id).await?;
        if stored.record.deleted || stored.record.version != request.version {
            return Err(Error::Conflict);
        }
        protect_ledger(&stored)?;
        self.fence_free(conn, request.record_id, Some(request.operation_id))
            .await?;
        stored.record.deleted = true;
        stored.record.version += 1;
        Self::save_record(conn, &stored).await?;
        sqlx::query("UPDATE native_memberships SET deleted=1,version=version+1 WHERE parent_id=? AND deleted=0").bind(request.record_id.to_string()).execute(&mut *conn).await.map_err(unavailable)?;
        sqlx::query("UPDATE native_deletions SET state='committed' WHERE operation_id=?")
            .bind(request.operation_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
        Ok(NativeReply::new("deleted"))
    }

    async fn metering_target(
        &self,
        conn: &mut SqliteConnection,
        target: &MeteringTarget,
    ) -> Result<(Uuid, Option<Stored>)> {
        match target {
            MeteringTarget::Object { lease } => {
                Ok((lease.record_id, Some(self.lease_record(conn, lease).await?)))
            }
            MeteringTarget::Operation {
                operation_id,
                api_key_id,
            } => {
                let (_, related, _, _) = Self::operation(conn, *operation_id).await?;
                if related.submission.api_key_id != *api_key_id {
                    return Err(Error::NotFound);
                }
                Ok((*operation_id, None))
            }
        }
    }

    async fn reconciliation_receipt<T: Serialize>(
        &self,
        conn: &mut SqliteConnection,
        id: Uuid,
        is_object: bool,
        request: &T,
    ) -> Result<NativeReply> {
        self.persist_metering_receipt(conn, id, is_object, request, None)
            .await
    }

    async fn persist_metering_receipt<T: Serialize>(
        &self,
        conn: &mut SqliteConnection,
        id: Uuid,
        is_object: bool,
        request: &T,
        explicit_state: Option<&str>,
    ) -> Result<NativeReply> {
        let value = serde_json::to_value(request).map_err(unavailable)?;
        let wire = json(&value)?;
        if wire.len() > 4 * 1024 * 1024 {
            return Err(Error::InvalidRequest);
        }
        let usage = value
            .get("usage")
            .or_else(|| value.get("observed_usage"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        let observations: Vec<aisix_core::native_metering::ObservedUsage> =
            serde_json::from_value(usage.clone()).map_err(unavailable)?;
        if observations.len() > MAX_ITEMS
            || observations.iter().any(|entry| {
                entry.entry_id.len() > 256
                    || entry.entry_id.chars().any(char::is_control)
                    || !valid_id(&entry.upstream_model)
            })
            || (!observations.is_empty() && validate_observed_usage(&observations).is_err())
        {
            return Err(Error::InvalidRequest);
        }
        if json(&usage)?.len() > 2 * 1024 * 1024 {
            return Err(Error::InvalidRequest);
        }
        let known = !observations.is_empty();
        let state = explicit_state.unwrap_or(if known {
            "awaiting_reconciliation"
        } else {
            "unknown"
        });
        let reason = if state == "outbox_pending" {
            None
        } else {
            Some(
                value
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(if known {
                        "unpriced_meter"
                    } else {
                        "missing_usage"
                    }),
            )
        };
        let status = value
            .get("terminal_status")
            .and_then(serde_json::Value::as_str);
        let stored = if is_object {
            Some(Self::stored(conn, id).await?)
        } else {
            None
        };
        let operation_id = stored
            .as_ref()
            .map_or(id, |stored| stored.submission.operation_id);
        let incoming_artifacts = value
            .get("artifacts")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let previous = sqlx::query("SELECT object_id,state,terminal_status,reason,usage,artifacts,receipt_version FROM native_metering_records WHERE operation_id=?")
            .bind(operation_id.to_string()).fetch_optional(&mut *conn).await.map_err(unavailable)?;
        let mut artifacts = previous
            .as_ref()
            .map(|row| {
                decode::<Vec<serde_json::Value>>(row.try_get("artifacts").map_err(unavailable)?)
            })
            .transpose()?
            .unwrap_or_default();
        for artifact in incoming_artifacts {
            if !artifacts.contains(&artifact) {
                artifacts.push(artifact);
            }
        }
        if json(&artifacts)?.len() > 2 * 1024 * 1024 {
            return Err(Error::InvalidRequest);
        }
        let mut version = 1i64;
        if let Some(previous) = &previous {
            let old_object: Option<String> = previous.try_get("object_id").map_err(unavailable)?;
            let old_reason: Option<&str> = previous.try_get("reason").map_err(unavailable)?;
            let old_state: &str = previous.try_get("state").map_err(unavailable)?;
            let promotion = old_object.is_none()
                && old_reason == Some("uncertain_submission")
                && stored.as_ref().is_some_and(|s| s.submission.asynchronous)
                && old_state != "settled";
            if !promotion && old_object != stored.as_ref().map(|s| s.record.record_id.to_string()) {
                return Err(Error::Conflict);
            }
            let old_status: Option<&str> =
                previous.try_get("terminal_status").map_err(unavailable)?;
            if old_status.is_some() && old_status != status {
                return Err(Error::Conflict);
            }
            let old_usage: serde_json::Value =
                decode(previous.try_get("usage").map_err(unavailable)?)?;
            let old_artifacts: Vec<serde_json::Value> =
                decode(previous.try_get("artifacts").map_err(unavailable)?)?;
            let same_measurement =
                measurement(&old_usage) == measurement(&usage) && old_artifacts == artifacts;
            let same = same_measurement && old_status == status;
            if matches!(old_state, "settled" | "outbox_pending") && !same
                || old_state == "awaiting_reconciliation"
                    && (!same_measurement || state == "unknown")
            {
                return Err(Error::Conflict);
            }
            if !promotion
                && (matches!(old_state, "settled" | "outbox_pending")
                    || old_state == "awaiting_reconciliation" && same && state != "outbox_pending"
                    || old_state == "unknown" && same && state == "unknown")
            {
                let mut reply =
                    NativeReply::new(if matches!(old_state, "settled" | "outbox_pending") {
                        "completed"
                    } else {
                        "reconciliation"
                    });
                reply.metering_state = Some(old_state.into());
                reply.reconciliation_reason = old_reason.map(str::to_owned);
                reply.operation_id = Some(operation_id);
                return Ok(reply);
            }
            version = previous
                .try_get::<i64, _>("receipt_version")
                .map_err(unavailable)?
                .checked_add(1)
                .ok_or(Error::Conflict)?;
        }
        // 每次新证据追加版本；已有明确计量和终态不能被后来的重放改写。
        sqlx::query("INSERT INTO native_metering_records(operation_id,object_id,state,terminal_status,reason,usage,artifacts,receipt_version) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(operation_id) DO UPDATE SET object_id=excluded.object_id,state=excluded.state,terminal_status=excluded.terminal_status,reason=excluded.reason,usage=excluded.usage,artifacts=excluded.artifacts,receipt_version=excluded.receipt_version")
            .bind(operation_id.to_string()).bind(stored.as_ref().map(|s|s.record.record_id.to_string())).bind(state).bind(status).bind(reason).bind(json(&usage)?).bind(json(&artifacts)?).bind(version).execute(&mut *conn).await.map_err(unavailable)?;
        sqlx::query(
            "INSERT INTO native_metering_receipts(operation_id,version,receipt) VALUES(?,?,?)",
        )
        .bind(operation_id.to_string())
        .bind(version)
        .bind(wire)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
        if state != "outbox_pending" {
            if let Some(mut stored) = stored {
                stored.record.metering_state = Some(state.into());
                stored.record.reconciliation_reason = reason.map(str::to_owned);
                let terminal = known
                    && status.is_some()
                    && !matches!(
                        reason,
                        Some(
                            "missing_usage"
                                | "unknown_usage_shape"
                                | "incomplete_observation"
                                | "uncertain_submission"
                        )
                    );
                if terminal && !stored.record.completed {
                    stored.record.completed = true;
                    stored.record.version += 1;
                }
                Self::save_record(conn, &stored).await?;
                if terminal {
                    sqlx::query("UPDATE native_objects SET pending=0 WHERE record_id=?")
                        .bind(id.to_string())
                        .execute(&mut *conn)
                        .await
                        .map_err(unavailable)?;
                } else {
                    sqlx::query("UPDATE native_objects SET poll_after=?,lease_worker=NULL,lease_until=NULL WHERE record_id=?").bind(now()+60_000).bind(id.to_string()).execute(&mut *conn).await.map_err(unavailable)?;
                }
            }
        }
        let mut reply = NativeReply::new(if state == "outbox_pending" {
            "completed"
        } else {
            "reconciliation"
        });
        reply.operation_id = Some(operation_id);
        reply.metering_state = Some(state.into());
        reply.reconciliation_reason = reason.map(str::to_owned);
        Ok(reply)
    }

    async fn typed_inner(
        &self,
        conn: &mut SqliteConnection,
        request: TypedCompleteRequest,
    ) -> Result<NativeReply> {
        validate_observed_usage(&request.usage).map_err(|_| Error::InvalidRequest)?;
        let (id, stored) = self.metering_target(conn, &request.target).await?;
        if let MeteringTarget::Operation { operation_id, .. } = &request.target {
            if Self::operation(conn, *operation_id)
                .await?
                .1
                .submission
                .asynchronous
            {
                return Err(Error::InvalidRequest);
            }
        }
        if request.terminal_status.is_empty() || request.terminal_status.len() > 64 {
            return Err(Error::InvalidRequest);
        }
        if let MeteringTarget::Object { lease } = &request.target {
            if !request.artifacts.is_empty() {
                self.observed_inner(
                    conn,
                    BindObservedArtifactsRequest {
                        lease: lease.clone(),
                        entries: request.artifacts.clone(),
                        complete: None,
                    },
                )
                .await?;
            }
            let source = stored.ok_or(Error::Unavailable)?;
            if request
                .usage
                .iter()
                .all(|u| matches!(u.meter, ObservedMeter::Tokens { .. }))
                && matches!(
                    source.submission.kind,
                    NativeObjectKind::Response
                        | NativeObjectKind::MessageBatch
                        | NativeObjectKind::EvalRun
                )
            {
                if request.usage.iter().any(|observation| {
                    let expected = source
                        .submission
                        .entries
                        .iter()
                        .find(|entry| entry.entry_id == observation.entry_id)
                        .map_or(source.submission.upstream_model.as_str(), |entry| {
                            entry.upstream_model.as_str()
                        });
                    observation.upstream_model != expected
                }) {
                    return Err(Error::InvalidRequest);
                }
                let usage = request
                    .usage
                    .iter()
                    .map(|u| match &u.meter {
                        ObservedMeter::Tokens { usage } => Ok(usage.clone()),
                        _ => Err(Error::InvalidRequest),
                    })
                    .collect::<Result<Vec<_>>>()?;
                let receipt = self
                    .persist_metering_receipt(conn, id, true, &request, Some("outbox_pending"))
                    .await?;
                if source.record.completed {
                    return Ok(receipt);
                }
                return self
                    .complete_inner(
                        conn,
                        CompleteRequest {
                            lease: lease.clone(),
                            terminal_status: request.terminal_status.clone(),
                            usage,
                            output_file_ids: vec![],
                        },
                    )
                    .await;
            }
        } else if !request.artifacts.is_empty() {
            return Err(Error::InvalidRequest);
        }
        self.reconciliation_receipt(
            conn,
            id,
            matches!(request.target, MeteringTarget::Object { .. }),
            &request,
        )
        .await
    }
}

fn unique_resource<T: aisix_core::resource::Resource>(
    entries: Vec<Arc<aisix_core::resource::ResourceEntry<T>>>,
    kind: &str,
    id: Uuid,
) -> Result<Arc<aisix_core::resource::ResourceEntry<T>>> {
    let mut matches = entries
        .into_iter()
        .filter(|entry| stable_resource_id(kind, &entry.id) == id);
    let entry = matches.next().ok_or(Error::NotFound)?;
    if matches.next().is_some() {
        return Err(Error::Conflict);
    }
    Ok(entry)
}

impl StandaloneNativeStore {
    async fn authorize_chain(
        &self,
        conn: &mut SqliteConnection,
        snapshot: &AisixSnapshot,
        caller: &Principal,
        stored: &Stored,
        consumer: Option<EndpointFamily>,
    ) -> Result<()> {
        self.authorize(snapshot, caller, stored, consumer)?;
        // 正式 Batch 结果文件保留整批模型限制和父对象存续；只查必要的一跳。
        if stored.submission.kind == NativeObjectKind::File {
            if let Some(parent_id) = stored.record.parent_id {
                let parent = Self::stored(conn, parent_id).await?;
                if parent.submission.kind == NativeObjectKind::MessageBatch {
                    if !account(
                        stored.record.origin.as_ref().ok_or(Error::Unavailable)?,
                        parent.record.origin.as_ref().ok_or(Error::Unavailable)?,
                    ) {
                        return Err(Error::NotFound);
                    }
                    self.authorize(snapshot, caller, &parent, consumer)?;
                }
            }
        }
        Ok(())
    }
}

fn protect_ledger(stored: &Stored) -> Result<()> {
    if !stored.record.completed
        && (matches!(
            stored.submission.kind,
            NativeObjectKind::MessageBatch
                | NativeObjectKind::FineTuningJob
                | NativeObjectKind::Video
                | NativeObjectKind::EvalRun
        ) || (stored.submission.kind == NativeObjectKind::Response
            && stored.submission.asynchronous))
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

fn measurement(usage: &serde_json::Value) -> serde_json::Value {
    let mut value = usage.clone();
    if let Some(entries) = value.as_array_mut() {
        entries.sort_by(|a, b| {
            a.get("entry_id")
                .and_then(serde_json::Value::as_str)
                .cmp(&b.get("entry_id").and_then(serde_json::Value::as_str))
        });
        for entry in entries {
            if let Some(fields) = entry.as_object_mut() {
                fields.remove("occurred_at");
            }
            if let Some(fields) = entry
                .pointer_mut("/meter/usage")
                .and_then(serde_json::Value::as_object_mut)
            {
                fields.remove("occurred_at");
            }
        }
    }
    value
}
