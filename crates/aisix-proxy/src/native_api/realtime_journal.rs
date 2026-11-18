//! 普通实时 WS 按供应商事实持久追加；会话汇总不能再次进入同一计费出口。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use aisix_core::native_metering::{
    AudioMeasurementBasis, MeasurementSource, ObservedMeter, ObservedUsage,
};
use aisix_core::native_objects::{
    BeginRequest, NativeEntry, NativeObjectKind, NativeProtocol, NativeReply, OriginBinding,
    TerminalUsage,
};
use aisix_core::native_realtime::{
    AppendLiveUsageRequest, CheckpointLiveSessionRequest, CreateLiveSessionRequest,
    LiveSessionLease, NativeCredential, RealtimePurpose, RealtimeTransport, UsageCoverage,
};
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::realtime_frame::{Frame, Kind};
use super::registry::{proxy_error, LiveUsageAuthority, NativeObjectRegistry, NativeRegistryError};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::state::ProxyState;

pub(crate) struct Journal {
    endpoint: &'static str,
    persistence: Option<Persistence>,
    upstream_model: String,
    transcription: bool,
    pending: VecDeque<AppendLiveUsageRequest>,
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    origin: Option<OriginBinding>,
    provider_key_id: String,
    entries: HashMap<String, AsrEntry>,
    held_model_keys: Vec<String>,
    model_reservations: Vec<aisix_ratelimit::MultiReservation>,
    seen_asr: HashSet<String>,
    secondary_cost_usd: f64,
    primary_snapshot: Arc<aisix_core::AisixSnapshot>,
    primary_usage: crate::realtime::SessionUsage,
    primary_event_id: String,
    active_asr: Option<String>,
    configured_asr: Option<Option<String>>,
    audio_admission: Option<crate::realtime::TokenStamp>,
    prepared_audio_admission: Option<crate::realtime::TokenStamp>,
    configuration_ack: &'static str,
    committed: VecDeque<Option<String>>,
    items: HashMap<(String, u64), String>,
}

struct Persistence {
    registry: Arc<dyn NativeObjectRegistry>,
    authority: LiveUsageAuthority,
    api_key_id: Uuid,
    lease: LiveSessionLease,
    lease_expires_at: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
}

struct AcceptedAsr {
    entry_id: String,
    requested_model: String,
    upstream_model: String,
}

struct AsrEntry {
    accepted: AcceptedAsr,
    model: Arc<aisix_core::ResourceEntry<aisix_core::Model>>,
    pk_id: String,
    token_keys: Vec<String>,
    snapshot: Arc<aisix_core::AisixSnapshot>,
    usage: crate::realtime::SessionUsage,
    event_id: String,
    cost_usd: f64,
}

/// 同一会话只持有一个续租 future；轮询被音频抢占时请求仍保留，不阻塞转发。
pub(crate) struct LeaseRenewal {
    registry: Arc<dyn NativeObjectRegistry>,
    lease: LiveSessionLease,
    api_key_id: Uuid,
    expires_at: tokio::time::Instant,
    deadline_at: tokio::time::Instant,
    interval: tokio::time::Interval,
    pending: Option<BoxFuture<'static, Result<NativeReply, NativeRegistryError>>>,
}

impl LeaseRenewal {
    fn instant(at: DateTime<Utc>) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        now.checked_add((at - Utc::now()).to_std().unwrap_or_default())
            .unwrap_or(now)
    }

    /// 每帧仅检查最后确认的真实租约期限，不作远程鉴权或写事务。
    pub(crate) fn refusal(&self) -> Option<ProxyError> {
        (tokio::time::Instant::now() >= self.expires_at).then_some(ProxyError::AccountUnavailable)
    }

    pub(crate) async fn next(&mut self) -> Result<(), ProxyError> {
        loop {
            if let Some(error) = self.refusal() {
                return Err(error);
            }
            if let Some(pending) = self.pending.as_mut() {
                let result = tokio::select! {
                    _ = tokio::time::sleep_until(self.expires_at) => {
                        return Err(ProxyError::AccountUnavailable);
                    }
                    result = pending => result,
                };
                self.pending = None;
                // 迟到回包不能让本地已到期的会话重新开始派发。
                if let Some(error) = self.refusal() {
                    return Err(error);
                }
                let reply = match result {
                    Ok(reply) => reply,
                    Err(NativeRegistryError::Unavailable) => continue,
                    Err(_) => return Err(ProxyError::InvalidApiKey),
                };
                let live = reply.live_session.ok_or(ProxyError::AccountUnavailable)?;
                if live.operation_id != self.lease.operation_id
                    || live.api_key_id != self.api_key_id
                    || live.lease.as_ref() != Some(&self.lease)
                {
                    return Err(ProxyError::AccountUnavailable);
                }
                if live.requires_stop || live.state != "live" {
                    return Err(ProxyError::InvalidApiKey);
                }
                let expiry = live
                    .lease_expires_at
                    .ok_or(ProxyError::AccountUnavailable)?;
                self.confirmed_expiry(expiry);
                if let Some(error) = self.refusal() {
                    return Err(error);
                }
                return Ok(());
            }
            tokio::select! {
                _ = tokio::time::sleep_until(self.expires_at) => {
                    return Err(ProxyError::AccountUnavailable);
                }
                _ = self.interval.tick() => {}
            }
            let registry = Arc::clone(&self.registry);
            let request = CheckpointLiveSessionRequest {
                lease: self.lease.clone(),
                terminal: false,
                coverage: UsageCoverage::Partial,
                entries: Vec::new(),
            };
            self.pending = Some(Box::pin(async move {
                registry.checkpoint_live_session(request).await
            }));
        }
    }

    /// 同代次较旧回包不能缩短另一次配置写入已确认的租期，也不能复活到期会话。
    pub(crate) fn confirmed_expiry(&mut self, expiry: DateTime<Utc>) {
        if self.refusal().is_none() {
            self.expires_at = self
                .expires_at
                .max(Self::instant(expiry))
                .min(self.deadline_at);
        }
    }

    /// Drop 取消仍在等待的续租，不留下独立 task 或跨会话引用环。
    pub(crate) fn cancel(&mut self) {
        self.pending = None;
    }
}

impl Journal {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn admit(
        state: &ProxyState,
        admission_snapshot: &Arc<aisix_core::AisixSnapshot>,
        auth: &AuthenticatedKey,
        origin: Option<&OriginBinding>,
        model_id: &str,
        provider_key_id: &str,
        requested_model: &str,
        upstream_model: &str,
        provider: &str,
        request_id: &str,
        client: &ClientContext,
        credential: Option<&NativeCredential>,
        purpose: RealtimePurpose,
        held_model_keys: Vec<String>,
        endpoint: &'static str,
    ) -> Result<Option<Self>, ProxyError> {
        let persistence = Self::admit_persistence(
            state,
            auth,
            origin,
            model_id,
            requested_model,
            upstream_model,
            provider,
            request_id,
            client,
            credential,
            purpose,
        )
        .await?;
        let origin = persistence.as_ref().and_then(|_| origin.cloned());
        Ok(Some(Self {
            endpoint,
            persistence,
            upstream_model: upstream_model.to_owned(),
            transcription: purpose == RealtimePurpose::Transcription,
            pending: VecDeque::new(),
            state: state.clone(),
            auth: auth.clone(),
            client: client.clone(),
            origin,
            provider_key_id: provider_key_id.to_owned(),
            entries: HashMap::new(),
            held_model_keys,
            model_reservations: Vec::new(),
            seen_asr: HashSet::new(),
            secondary_cost_usd: 0.0,
            primary_snapshot: Arc::clone(admission_snapshot),
            primary_usage: crate::realtime::SessionUsage::default(),
            primary_event_id: Uuid::new_v4().to_string(),
            active_asr: None,
            configured_asr: None,
            audio_admission: None,
            prepared_audio_admission: None,
            configuration_ack: "session.updated",
            committed: VecDeque::new(),
            items: HashMap::new(),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn admit_persistence(
        state: &ProxyState,
        auth: &AuthenticatedKey,
        origin: Option<&OriginBinding>,
        model_id: &str,
        requested_model: &str,
        upstream_model: &str,
        provider: &str,
        request_id: &str,
        client: &ClientContext,
        credential: Option<&NativeCredential>,
        purpose: RealtimePurpose,
    ) -> Result<Option<Persistence>, ProxyError> {
        let Some(registry) = state.native_objects.as_ref() else {
            return if credential.is_some() {
                Err(ProxyError::AccountUnavailable)
            } else {
                Ok(None)
            };
        };
        let authority = registry.live_usage_authority();
        if authority == LiveUsageAuthority::Unsupported {
            return if credential.is_some() {
                Err(ProxyError::AccountUnavailable)
            } else {
                Ok(None)
            };
        }
        // 动态来源保留旧 aggregate；不能冒充已配置的静态账号。
        let Some(origin) = origin else {
            return if credential.is_some() {
                Err(ProxyError::AccountUnavailable)
            } else {
                Ok(None)
            };
        };
        let api_key_id = registry
            .resource_id("apikeys", &auth.entry.id)
            .map_err(proxy_error)?;
        let requested_model_id = registry
            .resource_id("models", model_id)
            .map_err(proxy_error)?;
        let operation_id = Uuid::new_v4();
        let accepted = BeginRequest {
            usage_context: crate::usage_attr::native_usage_context(client, Default::default()),
            upstream_input_text: None,
            input_file_id: None,
            operation_id,
            api_key_id,
            protocol: NativeProtocol::Openai,
            kind: NativeObjectKind::RealtimeCall,
            origin: origin.clone(),
            parent_id: None,
            requested_model_id,
            requested_model: requested_model.to_owned(),
            upstream_model: upstream_model.to_owned(),
            upstream_provider: provider.to_owned(),
            operation: "realtime".to_owned(),
            request_id: request_id.to_owned(),
            asynchronous: true,
            requested_service_tier: None,
            entries: Vec::new(),
            observation_headers: Default::default(),
        };
        let reply = registry.begin(accepted).await.map_err(proxy_error)?;
        if reply.operation_id != Some(operation_id)
            || reply.origin.as_ref() != Some(origin)
            || reply.state != "pending_submission"
        {
            return Err(ProxyError::AccountUnavailable);
        }
        let purpose = credential.map_or(purpose, |credential| credential.purpose);
        let reply = registry
            .create_live_session(CreateLiveSessionRequest {
                operation_id,
                api_key_id,
                purpose,
                transport: RealtimeTransport::WebSocket,
                credential_id: credential.map(|credential| credential.credential_id),
                provider_session_id: None,
                deadline_at: Utc::now() + chrono::Duration::hours(1),
            })
            .await
            .map_err(proxy_error)?;
        let record = reply.live_session.ok_or(ProxyError::AccountUnavailable)?;
        let lease = record.lease.ok_or(ProxyError::AccountUnavailable)?;
        let lease_expires_at = record
            .lease_expires_at
            .ok_or(ProxyError::AccountUnavailable)?;
        let deadline_at = record.deadline_at.ok_or(ProxyError::AccountUnavailable)?;
        if record.operation_id != operation_id
            || record.api_key_id != api_key_id
            || record.transport != RealtimeTransport::WebSocket
            || record.purpose != purpose
            || record.requires_stop
            || lease.operation_id != operation_id
            || lease_expires_at <= Utc::now()
            || deadline_at <= Utc::now()
        {
            return Err(ProxyError::AccountUnavailable);
        }
        Ok(Some(Persistence {
            registry: Arc::clone(registry),
            authority,
            api_key_id,
            lease,
            lease_expires_at,
            deadline_at,
        }))
    }

    pub(crate) fn canonical(&self) -> bool {
        self.persistence
            .as_ref()
            .is_some_and(|p| p.authority == LiveUsageAuthority::CanonicalJournal)
    }

    pub(crate) fn operation_id(&self) -> Option<Uuid> {
        self.persistence.as_ref().map(|p| p.lease.operation_id)
    }

    pub(crate) fn lease_expires_at(&self) -> Option<DateTime<Utc>> {
        self.persistence.as_ref().map(|p| p.lease_expires_at)
    }

    pub(crate) fn lease_renewal(&self) -> Option<LeaseRenewal> {
        let p = self.persistence.as_ref()?;
        let deadline_at = LeaseRenewal::instant(p.deadline_at);
        let first = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut interval = tokio::time::interval_at(first, std::time::Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Some(LeaseRenewal {
            registry: Arc::clone(&p.registry),
            lease: p.lease.clone(),
            api_key_id: p.api_key_id,
            expires_at: LeaseRenewal::instant(p.lease_expires_at).min(deadline_at),
            deadline_at,
            interval,
            pending: None,
        })
    }

    pub(crate) fn secondary_cost_usd(&self) -> f64 {
        self.secondary_cost_usd
    }

    /// 无持久来源仍沿原 aggregate 队列；不同真实模型不能再合并按主模型计价。
    pub(crate) fn billing_rows(
        &self,
        summary: &aisix_obs::UsageEvent,
    ) -> Vec<(Arc<aisix_core::AisixSnapshot>, aisix_obs::UsageEvent)> {
        if !self.entries.values().any(|entry| entry.usage.has_reports()) {
            return Vec::new();
        }
        let mut rows = Vec::new();
        if self.primary_usage.has_reports() {
            let mut row = summary.clone();
            self.primary_usage.apply_to(&mut row);
            row.event_id = self.primary_event_id.clone();
            row.io_audit = None;
            row.cost_usd = (summary.cost_usd - self.secondary_cost_usd).max(0.0);
            rows.push((Arc::clone(&self.primary_snapshot), row));
        }
        for entry in self
            .entries
            .values()
            .filter(|entry| entry.usage.has_reports())
        {
            let mut row = summary.clone();
            entry.usage.apply_to(&mut row);
            row.event_id = entry.event_id.clone();
            row.io_audit = None;
            row.model_id = entry.model.id.clone();
            row.requested_model_id = entry.model.id.clone();
            row.requested_model = entry.accepted.requested_model.clone();
            row.cost_usd = entry.cost_usd;
            rows.push((Arc::clone(&entry.snapshot), row));
        }
        rows
    }

    fn asr_entry(&self, event: &Value) -> Option<&AsrEntry> {
        let id = event["item_id"].as_str()?;
        let index = event["content_index"].as_u64()?;
        self.items
            .get(&(id.to_owned(), index))
            .and_then(|entry| self.entries.get(entry))
    }

    pub(crate) fn is_secondary_fact(&self, frame: &Frame<'_>) -> bool {
        frame.kind() == Kind::TranscriptionCompleted
            && (!self.transcription
                || frame
                    .json()
                    .is_some_and(|event| self.asr_entry(event).is_some()))
    }

    pub(crate) fn configuration_ack(&self) -> Option<&'static str> {
        self.configured_asr
            .is_some()
            .then_some(self.configuration_ack)
    }

    /// ASR 是独立收费模型；转发配置前先固定当前授权身份，不能借主模型的准入。
    pub(crate) async fn prepare_frame(
        &mut self,
        frame: &mut Frame<'_>,
        checked: &mut crate::quota::RealtimeTokenChecks,
    ) -> Result<Option<Vec<u8>>, ProxyError> {
        self.prepared_audio_admission = None;
        let kind = frame.kind();
        // 未启用 ASR 时 item 无次级绑定；主 A/request 闸仍由共享入口执行。
        if kind == Kind::ItemCreate && self.active_asr.is_none() {
            return Ok(None);
        }
        let audio_input = matches!(kind, Kind::AudioAppend | Kind::AudioCommit)
            || (kind == Kind::ItemCreate
                && frame.json().is_some_and(|event| {
                    event["item"]["content"].as_array().is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| part["type"].as_str() == Some("input_audio"))
                    })
                }));
        if audio_input {
            if let Some(entry) = self.active_asr.as_ref().and_then(|id| self.entries.get(id)) {
                let snapshot = self.state.snapshot.load();
                let current = super::openai::current_auth(&snapshot, &self.auth)?;
                let model = snapshot
                    .models
                    .get_by_id(&entry.model.id)
                    .ok_or(ProxyError::AccountUnavailable)?;
                // 配置同步后仍须允许该次级模型；主模型 A 可用不能代替 B 的准入。
                if !current
                    .key()
                    .can_access(&snapshot, &entry.accepted.requested_model)
                {
                    return Err(ProxyError::ModelForbidden(
                        entry.accepted.requested_model.clone(),
                    ));
                }
                if !crate::routing::target_in_groups(
                    &snapshot,
                    &model.value,
                    &current.key().allowed_provider_groups,
                ) {
                    return Err(crate::routing::no_target_in_groups(
                        &entry.accepted.requested_model,
                    ));
                }
                crate::dispatch::check_ip_access(&model.value, &self.client.source_ip)?;
                let buffer = matches!(kind, Kind::AudioAppend | Kind::AudioCommit);
                if !buffer
                    || !self
                        .audio_admission
                        .as_ref()
                        .is_some_and(|stamp| stamp.matches(&snapshot, &current, &model))
                {
                    let stamp = buffer
                        .then(|| crate::realtime::TokenStamp::capture(&snapshot, &current, &model));
                    crate::quota::check_realtime_tokens_once(
                        &self.state,
                        &snapshot,
                        &current,
                        &crate::quota::ModelRateLimit::from_model(
                            &entry.accepted.requested_model,
                            &model.id,
                            &model.value,
                        ),
                        false,
                        checked,
                    )
                    .await?;
                    self.prepared_audio_admission = stamp;
                }
            }
        }
        if kind == Kind::AudioCommit {
            if self.committed.len() >= 128 {
                return Err(ProxyError::AccountUnavailable);
            }
            self.committed.push_back(self.active_asr.clone());
            return Ok(None);
        }
        if kind == Kind::ItemCreate {
            if let Some(event) = frame.json() {
                self.bind_item(&event["item"])?;
            }
            return Ok(None);
        }
        if !matches!(kind, Kind::SessionUpdate | Kind::TranscriptionUpdate) {
            return Ok(None);
        }
        let Some(event) = frame.json_mut() else {
            return Ok(None);
        };
        let pointer = transcription_pointer(event);
        let Some(pointer) = pointer else {
            return Ok(None);
        };
        let transcription = event.pointer(pointer).cloned().unwrap_or(Value::Null);
        self.configuration_ack = if kind == Kind::TranscriptionUpdate {
            "transcription_session.updated"
        } else {
            "session.updated"
        };
        if transcription.is_null() {
            self.configured_asr = Some(None);
            return Ok(None);
        }
        let Some(wanted) = transcription.get("model").and_then(Value::as_str) else {
            return Ok(None);
        };
        let snapshot = self.state.snapshot.load();
        let current = super::openai::current_auth(&snapshot, &self.auth)?;
        let (model, pk, actual) = if let Some(origin) = self.origin.as_ref() {
            let (target, actual) = crate::attribution::detached(async {
                super::openai::resolve_for_requested(
                    &self.state,
                    &snapshot,
                    &current,
                    &self.client,
                    wanted,
                    wanted,
                    NativeObjectKind::RealtimeCall,
                )
            })
            .await?;
            if !super::skills::same_account(origin, &actual) {
                return Err(super::skills::not_found());
            }
            (target.model_entry, target.pk_entry, Some(actual))
        } else {
            // 本地 ASR 准入独立于持久来源；仍只使用已打开套接字的账号。
            crate::endpoint_acl::check_endpoint(
                current.key(),
                aisix_core::models::EndpointFamily::Realtime,
            )?;
            if !current.key().can_access(&snapshot, wanted) {
                return Err(ProxyError::ModelForbidden(wanted.to_owned()));
            }
            let model = crate::model_resolve::resolve_model(&snapshot, wanted)
                .ok_or_else(|| ProxyError::ModelNotFound(wanted.to_owned()))?;
            if model.value.is_routing() || model.value.is_ensemble() || model.value.is_semantic() {
                return Err(ProxyError::InvalidRequest(
                    "Realtime ASR requires a direct model".into(),
                ));
            }
            if !crate::routing::target_in_groups(
                &snapshot,
                &model.value,
                &current.key().allowed_provider_groups,
            ) {
                return Err(crate::routing::no_target_in_groups(wanted));
            }
            crate::dispatch::check_ip_access(&model.value, &self.client.source_ip)?;
            let pk = crate::dispatch::resolve_provider_key(&snapshot, &model.value)?;
            if pk.id != self.provider_key_id {
                return Err(ProxyError::AccountUnavailable);
            }
            crate::budget::gate(&self.state, &snapshot).check_provider_key(&pk.id)?;
            (model, pk, None)
        };
        crate::budget::enforce_key(&self.state, &snapshot, &current)?;
        let upstream_model = crate::dispatch::require_upstream_model(&model.value)?.to_owned();
        let mut identity = Sha256::new();
        for field in [wanted, model.id.as_str(), upstream_model.as_str()] {
            identity.update((field.len() as u64).to_be_bytes());
            identity.update(field.as_bytes());
        }
        identity.update(model.revision.to_be_bytes());
        let entry_id = format!("asr:{:x}", identity.finalize());
        if self.entries.len() >= 128 && !self.entries.contains_key(&entry_id) {
            return Err(ProxyError::AccountUnavailable);
        }
        crate::quota::check_realtime_tokens(
            &self.state,
            &snapshot,
            &current,
            &crate::quota::ModelRateLimit::from_model(wanted, &model.id, &model.value),
            false,
        )
        .await?;
        let (reservation, token_keys) = if let Some(entry) = self.entries.get(&entry_id) {
            (None, entry.token_keys.clone())
        } else {
            let (reservation, keys) = crate::quota::reserve_realtime_model(
                &self.state,
                &snapshot,
                &current,
                wanted,
                &model.id,
                &model.value,
                &self.held_model_keys,
            )
            .await?;
            (Some(reservation), keys)
        };
        let native = if let Some(actual) = actual {
            Some(NativeEntry {
                usage_context: None,
                upstream_input_text: None,
                endpoint: "/v1/audio/transcriptions".to_owned(),
                entry_id: entry_id.clone(),
                requested_model: wanted.to_owned(),
                requested_service_tier: None,
                requested_model_id: super::openai::resource_id(&self.state, "models", &model.id)?,
                model_id: actual.model_id,
                upstream_model: upstream_model.clone(),
                upstream_provider: pk.value.provider.clone(),
            })
        } else {
            None
        };
        if let (Some(p), Some(entry)) = (self.persistence.as_mut(), native.as_ref()) {
            let reply = p
                .registry
                .checkpoint_live_session(CheckpointLiveSessionRequest {
                    lease: p.lease.clone(),
                    terminal: false,
                    coverage: UsageCoverage::Partial,
                    entries: vec![entry.clone()],
                })
                .await
                .map_err(proxy_error)?;
            let live = reply
                .live_session
                .as_ref()
                .ok_or(ProxyError::AccountUnavailable)?;
            if live.operation_id != p.lease.operation_id
                || live.api_key_id != p.api_key_id
                || live.lease.as_ref() != Some(&p.lease)
                || live.requires_stop
            {
                return Err(ProxyError::InvalidApiKey);
            }
            if let Some(expiry) = live.lease_expires_at {
                p.lease_expires_at = p.lease_expires_at.max(expiry);
            }
        }
        if let Some(reservation) = reservation {
            self.held_model_keys.extend(reservation.keys());
            self.held_model_keys.sort_unstable();
            self.held_model_keys.dedup();
            self.model_reservations.push(reservation);
        }
        self.entries
            .entry(entry_id.clone())
            .or_insert_with(|| AsrEntry {
                accepted: AcceptedAsr {
                    entry_id: entry_id.clone(),
                    requested_model: wanted.to_owned(),
                    upstream_model: upstream_model.clone(),
                },
                model: Arc::clone(&model),
                pk_id: pk.id.clone(),
                token_keys,
                snapshot: Arc::clone(&snapshot),
                usage: crate::realtime::SessionUsage::default(),
                event_id: Uuid::new_v4().to_string(),
                cost_usd: 0.0,
            });
        self.configured_asr = Some(Some(entry_id));
        if let Some(slot) = event.pointer_mut(&format!("{pointer}/model")) {
            *slot = Value::String(upstream_model);
        }
        serde_json::to_vec(&*event)
            .map(Some)
            .map_err(|_| ProxyError::AccountUnavailable)
    }

    /// 额度许可只在真实发送成功后生效；commit/clear 不替下一 buffer 授权。
    pub(crate) fn confirmed_input(&mut self, kind: Kind) {
        match kind {
            Kind::AudioAppend => {
                if let Some(stamp) = self.prepared_audio_admission.take() {
                    self.audio_admission = Some(stamp);
                }
            }
            Kind::AudioCommit | Kind::AudioClear => self.audio_admission = None,
            _ => {}
        }
        self.prepared_audio_admission = None;
    }

    fn bind_item(&mut self, item: &Value) -> Result<(), ProxyError> {
        let (Some(id), Some(content), Some(entry)) = (
            item["id"].as_str(),
            item["content"].as_array(),
            self.active_asr.as_ref(),
        ) else {
            return Ok(());
        };
        if self.items.len() >= 16_384 || id.len() > 512 {
            return Err(ProxyError::AccountUnavailable);
        }
        for (index, part) in content.iter().enumerate() {
            if part["type"].as_str() == Some("input_audio") {
                self.items
                    .entry((id.to_owned(), index as u64))
                    .or_insert_with(|| entry.clone());
            }
        }
        Ok(())
    }

    fn observe_configuration(&mut self, event: &Value) -> Result<(), ProxyError> {
        match event["type"].as_str() {
            Some("session.updated" | "transcription_session.updated") => {
                if let Some(expected) = self.configured_asr.take() {
                    let actual = transcription_pointer(event)
                        .and_then(|pointer| event.pointer(pointer))
                        .and_then(|config| config["model"].as_str());
                    let matches = expected
                        .as_ref()
                        .map(|id| {
                            self.entries
                                .get(id)
                                .map(|entry| entry.accepted.upstream_model.as_str())
                        })
                        .unwrap_or(None)
                        == actual;
                    if !matches {
                        return Err(ProxyError::AccountUnavailable);
                    }
                    self.active_asr = expected;
                    self.audio_admission = None;
                }
            }
            Some("input_audio_buffer.cleared") => self.audio_admission = None,
            Some("input_audio_buffer.committed") => {
                self.audio_admission = None;
                let entry = self
                    .committed
                    .pop_front()
                    .unwrap_or_else(|| self.active_asr.clone());
                if let (Some(id), Some(entry)) = (event["item_id"].as_str(), entry) {
                    if self.items.len() >= 16_384 || id.len() > 512 {
                        return Err(ProxyError::AccountUnavailable);
                    }
                    self.items.entry((id.to_owned(), 0)).or_insert(entry);
                }
            }
            Some("conversation.item.created" | "conversation.item.added") => {
                self.bind_item(&event["item"])?
            }
            _ => {}
        }
        Ok(())
    }

    fn observe_asr_meter(&mut self, frame: &Frame<'_>, event: &Value) -> Result<(), ProxyError> {
        if event["type"].as_str() != Some("conversation.item.input_audio_transcription.completed")
            || self.asr_entry(event).is_none()
        {
            return Ok(());
        }
        let Some(measured) = terminal_usage(&event["usage"], Some("completed")) else {
            return Ok(());
        };
        let Some((id, index)) = event["item_id"]
            .as_str()
            .zip(event["content_index"].as_u64())
        else {
            return Ok(());
        };
        let fact = format!("transcription:{:x}:{index}", Sha256::digest(id.as_bytes()));
        let input = u64::from(measured.prompt_tokens);
        let output = u64::from(measured.completion_tokens);
        let cached = u64::from(measured.cached_prompt_tokens);
        if self.seen_asr.insert(fact) {
            if input.saturating_add(output) > 0 {
                self.audio_admission = None;
            }
            let entry = self
                .asr_entry(event)
                .ok_or(ProxyError::AccountUnavailable)?;
            self.state
                .limiter
                .add_tokens_post_stream_all(&entry.token_keys, input + output);
            let snapshot = Arc::clone(&entry.snapshot);
            let pk = crate::usage_attr::ResolvedPk::resolve(&snapshot, &entry.pk_id);
            crate::request_metrics::record_usage(
                &self.state,
                self.endpoint,
                crate::request_metrics::Caller::new(&self.auth),
                crate::request_metrics::Upstream {
                    provider: entry.model.value.provider.as_deref().unwrap_or_default(),
                    model: &entry.accepted.requested_model,
                    upstream_model: &entry.accepted.upstream_model,
                    pk: pk.labels(),
                    ..Default::default()
                },
                crate::request_metrics::Tokens {
                    input: measured.prompt_tokens,
                    output: measured.completion_tokens,
                    total: measured
                        .prompt_tokens
                        .saturating_add(measured.completion_tokens),
                    cached: measured.cached_prompt_tokens,
                    cache_read: 0,
                    cache_creation: 0,
                    client_type: self
                        .state
                        .client_classifier
                        .classify(&self.client.user_agent),
                },
            );
            let cost = self
                .state
                .pricing
                .for_snapshot(&snapshot)
                .resolve(&entry.model.value)
                .map(|price| {
                    price.calculate_with_cache(
                        crate::usage_attr::input_tokens_for_pricing(input, cached, 0, 0, 0),
                        output,
                    )
                })
                .unwrap_or(0.0);
            let entry_id = entry.accepted.entry_id.clone();
            self.secondary_cost_usd += cost;
            if let Some(entry) = self.entries.get_mut(&entry_id) {
                entry.cost_usd += cost;
                entry.usage.absorb_frame(frame, false);
            }
        }
        Ok(())
    }

    /// 相同供应商事实换 event_id 重投只观察一次；不以传输投递 ID 定义收费事实。
    pub(crate) fn collect(&mut self, frame: &Frame<'_>) -> Result<bool, ProxyError> {
        if !matches!(
            frame.kind(),
            Kind::SessionUpdated
                | Kind::TranscriptionUpdated
                | Kind::AudioCommitted
                | Kind::AudioCleared
                | Kind::ItemCreated
                | Kind::TranscriptionCompleted
                | Kind::ResponseDone
        ) {
            return Ok(true);
        }
        let Some(event) = frame.json() else {
            return Ok(true);
        };
        self.observe_configuration(event)?;
        let secondary = self.is_secondary_fact(frame);
        self.observe_asr_meter(frame, event)?;
        let primary_usage = if frame.kind() == Kind::ResponseDone {
            &event["response"]["usage"]
        } else {
            &event["usage"]
        };
        // 未报告的主量只留在会话观察；不能因 B 有量而新增 A 的假零账。
        if !secondary {
            if let Some((input, output)) = reported_token_quantities(primary_usage) {
                let before = self.primary_usage.report_count();
                self.primary_usage.absorb_frame(frame, false);
                if self.primary_usage.report_count() != before && input.saturating_add(output) > 0 {
                    self.audio_admission = None;
                }
            }
        }
        let Some(p) = self.persistence.as_ref() else {
            return Ok(true);
        };
        if event["type"].as_str() == Some("conversation.item.input_audio_transcription.completed") {
            let identity = event["item_id"]
                .as_str()
                .zip(event["content_index"].as_u64());
            let entry = self.asr_entry(event);
            let Some(entry) = entry else {
                return self.unknown_usage(&event["usage"]);
            };
            // 供应商转录可按输入秒数报告，包括显式 0；不将缺价秒量改成 token 或零账。
            let meter = if event["usage"]["type"].as_str() == Some("duration") {
                event["usage"]["seconds"].as_number().and_then(|number| {
                    let mut seconds = number.to_string();
                    if seconds.contains(['e', 'E']) {
                        seconds = number.as_f64()?.to_string();
                    }
                    aisix_core::native_metering::validate_reported_seconds(&seconds).ok()?;
                    Some(ObservedMeter::AudioSeconds {
                        seconds,
                        basis: AudioMeasurementBasis::InputAudioSeconds,
                        source: MeasurementSource::SupplierReported,
                    })
                })
            } else {
                terminal_usage(&event["usage"], Some("completed")).map(|mut measured| {
                    measured.entry_id = entry.accepted.entry_id.clone();
                    ObservedMeter::Tokens { usage: measured }
                })
            };
            let Some(meter) = meter else {
                return self.unknown_usage(&event["usage"]);
            };
            let Some((id, index)) = identity else {
                return Ok(true);
            };
            let fact = format!("transcription:{:x}:{index}", Sha256::digest(id.as_bytes()));
            let entry_id = entry.accepted.entry_id.clone();
            let upstream_model = entry.accepted.upstream_model.clone();
            let at = match &meter {
                ObservedMeter::Tokens { usage } => usage.occurred_at,
                _ => Utc::now(),
            };
            self.pending.push_back(AppendLiveUsageRequest {
                operation_id: p.lease.operation_id,
                api_key_id: p.api_key_id,
                event_id: fact.clone(),
                response_id: Some(fact),
                usage: vec![ObservedUsage {
                    entry_id,
                    upstream_model,
                    occurred_at: at,
                    meter,
                }],
                coverage: UsageCoverage::Complete,
                terminal: false,
                raw_usage: Some(event["usage"].clone()),
                lease: Some(p.lease.clone()),
            });
            return Ok(true);
        }
        if event.get("type").and_then(Value::as_str) != Some("response.done") {
            return Ok(true);
        }
        let response = &event["response"];
        let id = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control));
        let usage = &response["usage"];
        if !usage.is_object() {
            // 未报告用量不编造零账；最终 checkpoint 保留 unknown。
            return Ok(true);
        }
        let measured = terminal_usage(usage, response["status"].as_str());
        if id.is_none() || measured.is_none() {
            // SDK 的 response.id 可缺失；只保存待对账观察，不虚构收费事实或拒掉旧 WS。
            self.pending.push_back(AppendLiveUsageRequest {
                operation_id: p.lease.operation_id,
                api_key_id: p.api_key_id,
                event_id: format!("gateway-observation:{}", Uuid::new_v4()),
                response_id: None,
                usage: Vec::new(),
                coverage: UsageCoverage::Unknown,
                terminal: false,
                raw_usage: Some(usage.clone()),
                lease: Some(p.lease.clone()),
            });
            return Ok(true);
        }
        let (Some(id), Some(measured)) = (id, measured) else {
            return Ok(true);
        };
        let at = measured.occurred_at;
        self.pending.push_back(AppendLiveUsageRequest {
            operation_id: p.lease.operation_id,
            api_key_id: p.api_key_id,
            event_id: format!("response:{id}"),
            response_id: Some(id.to_owned()),
            usage: vec![ObservedUsage {
                entry_id: String::new(),
                upstream_model: self.upstream_model.clone(),
                occurred_at: at,
                meter: ObservedMeter::Tokens { usage: measured },
            }],
            coverage: UsageCoverage::Complete,
            terminal: false,
            raw_usage: Some(usage.clone()),
            lease: Some(p.lease.clone()),
        });
        Ok(true)
    }

    fn unknown_usage(&mut self, usage: &Value) -> Result<bool, ProxyError> {
        let Some(p) = self.persistence.as_ref() else {
            return Ok(true);
        };
        if !usage.is_object() {
            return Ok(true);
        }
        self.pending.push_back(AppendLiveUsageRequest {
            operation_id: p.lease.operation_id,
            api_key_id: p.api_key_id,
            event_id: format!("gateway-observation:{}", Uuid::new_v4()),
            response_id: None,
            usage: Vec::new(),
            coverage: UsageCoverage::Unknown,
            terminal: false,
            raw_usage: Some(usage.clone()),
            lease: Some(p.lease.clone()),
        });
        Ok(true)
    }

    pub(crate) fn pending_full(&self) -> bool {
        self.pending.len() >= 128
    }

    /// 已读到的事实先入队；未知追加结果只重试原稳定身份，不能退回会话汇总收费。
    pub(crate) async fn flush(&mut self) -> Result<(), ProxyError> {
        let Some(p) = self.persistence.as_ref() else {
            return Ok(());
        };
        while let Some(request) = self.pending.front().cloned() {
            let registry = Arc::clone(&p.registry);
            let reply = registry
                .append_live_usage(request)
                .await
                .map_err(proxy_error)?;
            if reply.operation_id != Some(p.lease.operation_id) || reply.appended.is_none() {
                return Err(ProxyError::AccountUnavailable);
            }
            self.pending.pop_front();
        }
        Ok(())
    }

    pub(crate) async fn finish(&mut self) -> Result<(), ProxyError> {
        // 套接字已停止后才有限重试，不把持久化等待纳入两秒读排空预算。
        let persisted = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            for attempt in 0..3 {
                match self.flush().await {
                    Ok(()) => return Ok(()),
                    Err(error) if attempt == 2 => return Err(error),
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
                }
            }
            Err(ProxyError::AccountUnavailable)
        })
        .await
        .unwrap_or(Err(ProxyError::AccountUnavailable));
        let Some(p) = self.persistence.as_ref() else {
            return persisted;
        };
        if persisted.is_err() {
            tracing::warn!(
                operation_id = %p.lease.operation_id,
                pending_facts = self.pending.len(),
                reason = "usage_persistence_unavailable",
                "realtime closing usage remains unconfirmed"
            );
        }
        // 即使追加失败也尝试收束 Unknown；不得把未确认的 RAM 数量称为已持久或零费用。
        let closed = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            p.registry
                .checkpoint_live_session(CheckpointLiveSessionRequest {
                    lease: p.lease.clone(),
                    terminal: true,
                    coverage: UsageCoverage::Unknown,
                    entries: Vec::new(),
                }),
        )
        .await
        .map_err(|_| ProxyError::AccountUnavailable)?
        .map_err(proxy_error);
        persisted.and(closed.map(|_| ()))
    }
}

fn transcription_pointer(event: &Value) -> Option<&'static str> {
    [
        "/session/audio/input/transcription",
        "/session/input_audio_transcription",
    ]
    .into_iter()
    .find(|pointer| event.pointer(pointer).is_some())
}

fn reported_token_quantities(usage: &Value) -> Option<(u32, u32)> {
    Some((
        u32::try_from(usage.get("input_tokens")?.as_u64()?).ok()?,
        u32::try_from(usage.get("output_tokens")?.as_u64()?).ok()?,
    ))
}

fn terminal_usage(usage: &Value, status: Option<&str>) -> Option<TerminalUsage> {
    let (prompt_tokens, completion_tokens) = reported_token_quantities(usage)?;
    let count = |value: Option<&Value>| {
        value
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
    };
    let input = usage.get("input_token_details");
    let output = usage.get("output_token_details");
    let cached = count(input.and_then(|details| details.get("cached_tokens")));
    let cancelled = status == Some("cancelled");
    Some(TerminalUsage {
        io_audit: None,
        terminal_status: status.map(str::to_owned),
        error_class: String::new(),
        error_message: String::new(),
        entry_id: String::new(),
        occurred_at: Utc::now(),
        status_code: if cancelled { 499 } else { 200 },
        prompt_tokens,
        completion_tokens,
        cached_prompt_tokens: cached.unwrap_or(0),
        cache_usage_reported: Some(cached.is_some()),
        cache_write_tokens: None,
        cache_write_30m_tokens: None,
        cache_write_12h_tokens: None,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_5m_tokens: None,
        cache_creation_1h_tokens: None,
        reasoning_tokens: 0,
        total_tokens: count(usage.get("total_tokens")).unwrap_or(0),
        prompt_audio_tokens: count(input.and_then(|details| details.get("audio_tokens"))),
        prompt_image_tokens: count(input.and_then(|details| details.get("image_tokens"))),
        completion_audio_tokens: count(output.and_then(|details| details.get("audio_tokens"))),
        completion_image_tokens: count(output.and_then(|details| details.get("image_tokens"))),
        web_search_requests: 0,
        usage_estimated: false,
        service_tier: None,
        requested_service_tier: None,
    })
}
