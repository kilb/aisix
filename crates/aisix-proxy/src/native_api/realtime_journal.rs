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

use super::registry::{proxy_error, LiveUsageAuthority, NativeObjectRegistry, NativeRegistryError};
use crate::auth::AuthenticatedKey;
use crate::client_ip::ClientContext;
use crate::error::ProxyError;
use crate::state::ProxyState;

pub(crate) struct Journal {
    endpoint: &'static str,
    registry: Arc<dyn NativeObjectRegistry>,
    authority: LiveUsageAuthority,
    api_key_id: Uuid,
    lease: LiveSessionLease,
    lease_expires_at: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
    upstream_model: String,
    transcription: bool,
    pending: Option<AppendLiveUsageRequest>,
    state: ProxyState,
    auth: AuthenticatedKey,
    client: ClientContext,
    origin: OriginBinding,
    entries: HashMap<String, AsrEntry>,
    held_model_keys: Vec<String>,
    model_reservations: Vec<aisix_ratelimit::MultiReservation>,
    seen_asr: HashSet<String>,
    secondary_cost_usd: f64,
    active_asr: Option<String>,
    configured_asr: Option<Option<String>>,
    configuration_ack: &'static str,
    committed: VecDeque<Option<String>>,
    items: HashMap<(String, u64), String>,
}

struct AsrEntry {
    accepted: NativeEntry,
    model: Arc<aisix_core::ResourceEntry<aisix_core::Model>>,
    pk_id: String,
    token_keys: Vec<String>,
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
        held_model_keys: Vec<String>,
        endpoint: &'static str,
    ) -> Result<Option<Self>, ProxyError> {
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
        // 动态来源将走独立的只观察证明；不能冒充已配置静态账号来源。
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
        Ok(Some(Self {
            endpoint,
            registry: Arc::clone(registry),
            authority,
            api_key_id,
            lease,
            lease_expires_at,
            deadline_at,
            upstream_model: upstream_model.to_owned(),
            transcription: purpose == RealtimePurpose::Transcription,
            pending: None,
            state: state.clone(),
            auth: auth.clone(),
            client: client.clone(),
            origin: origin.clone(),
            entries: HashMap::new(),
            held_model_keys,
            model_reservations: Vec::new(),
            seen_asr: HashSet::new(),
            secondary_cost_usd: 0.0,
            active_asr: None,
            configured_asr: None,
            configuration_ack: "session.updated",
            committed: VecDeque::new(),
            items: HashMap::new(),
        }))
    }

    pub(crate) fn canonical(&self) -> bool {
        self.authority == LiveUsageAuthority::CanonicalJournal
    }

    pub(crate) fn lease_expires_at(&self) -> DateTime<Utc> {
        self.lease_expires_at
    }

    pub(crate) fn lease_renewal(&self) -> LeaseRenewal {
        let deadline_at = LeaseRenewal::instant(self.deadline_at);
        let first = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut interval = tokio::time::interval_at(first, std::time::Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        LeaseRenewal {
            registry: Arc::clone(&self.registry),
            lease: self.lease.clone(),
            api_key_id: self.api_key_id,
            expires_at: LeaseRenewal::instant(self.lease_expires_at).min(deadline_at),
            deadline_at,
            interval,
            pending: None,
        }
    }

    pub(crate) fn secondary_cost_usd(&self) -> f64 {
        self.secondary_cost_usd
    }

    fn asr_entry(&self, event: &Value) -> Option<&AsrEntry> {
        let id = event["item_id"].as_str()?;
        let index = event["content_index"].as_u64()?;
        self.items
            .get(&(id.to_owned(), index))
            .and_then(|entry| self.entries.get(entry))
    }

    pub(crate) fn is_secondary_fact(&self, text: &str) -> bool {
        serde_json::from_str::<Value>(text).is_ok_and(|event| {
            event["type"].as_str() == Some("conversation.item.input_audio_transcription.completed")
                && (!self.transcription || self.asr_entry(&event).is_some())
        })
    }

    pub(crate) fn configuration_ack(&self) -> Option<&'static str> {
        self.configured_asr
            .is_some()
            .then_some(self.configuration_ack)
    }

    /// ASR 是独立收费模型；转发配置前先固定当前授权身份，不能借主模型的准入。
    pub(crate) async fn prepare_frame(
        &mut self,
        bytes: &[u8],
    ) -> Result<Option<Vec<u8>>, ProxyError> {
        let Ok(mut event) = serde_json::from_slice::<Value>(bytes) else {
            return Ok(None);
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let audio_input = matches!(
            kind,
            "input_audio_buffer.append" | "input_audio_buffer.commit"
        ) || (kind == "conversation.item.create"
            && event["item"]["content"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part["type"].as_str() == Some("input_audio"))
            }));
        if audio_input {
            if let Some(entry) = self.active_asr.as_ref().and_then(|id| self.entries.get(id)) {
                let snapshot = self.state.snapshot.load();
                let current = super::openai::current_auth(&snapshot, &self.auth)?;
                let model = snapshot
                    .models
                    .get_by_id(&entry.model.id)
                    .ok_or(ProxyError::AccountUnavailable)?;
                crate::quota::check_realtime_tokens(
                    &self.state,
                    &snapshot,
                    &current,
                    &crate::quota::ModelRateLimit::from_model(
                        &entry.accepted.requested_model,
                        &model.id,
                        &model.value,
                    ),
                    false,
                )
                .await?;
            }
        }
        if kind == "input_audio_buffer.commit" {
            if self.committed.len() >= 128 {
                return Err(ProxyError::AccountUnavailable);
            }
            self.committed.push_back(self.active_asr.clone());
            return Ok(None);
        }
        if kind == "conversation.item.create" {
            self.bind_item(&event["item"])?;
            return Ok(None);
        }
        if !matches!(kind, "session.update" | "transcription_session.update") {
            return Ok(None);
        }
        let pointer = transcription_pointer(&event);
        let Some(pointer) = pointer else {
            return Ok(None);
        };
        let transcription = event.pointer(pointer).cloned().unwrap_or(Value::Null);
        self.configuration_ack = if kind == "transcription_session.update" {
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
        if !super::skills::same_account(&self.origin, &actual) {
            return Err(super::skills::not_found());
        }
        crate::budget::enforce_key(&self.state, &snapshot, &current)?;
        let upstream_model =
            crate::dispatch::require_upstream_model(&target.model_entry.value)?.to_owned();
        let entry_id = format!("asr:{:x}", Sha256::digest(wanted.as_bytes()));
        crate::quota::check_realtime_tokens(
            &self.state,
            &snapshot,
            &current,
            &crate::quota::ModelRateLimit::from_model(
                wanted,
                &target.model_entry.id,
                &target.model_entry.value,
            ),
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
                &target.model_entry.id,
                &target.model_entry.value,
                &self.held_model_keys,
            )
            .await?;
            (Some(reservation), keys)
        };
        let entry = NativeEntry {
            upstream_input_text: None,
            endpoint: "/v1/audio/transcriptions".to_owned(),
            entry_id: entry_id.clone(),
            requested_model: wanted.to_owned(),
            requested_service_tier: None,
            requested_model_id: super::openai::resource_id(
                &self.state,
                "models",
                &target.model_entry.id,
            )?,
            model_id: actual.model_id,
            upstream_model: upstream_model.clone(),
            upstream_provider: target.pk_entry.value.provider.clone(),
        };
        let reply = self
            .registry
            .checkpoint_live_session(CheckpointLiveSessionRequest {
                lease: self.lease.clone(),
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
        if live.operation_id != self.lease.operation_id
            || live.api_key_id != self.api_key_id
            || live.lease.as_ref() != Some(&self.lease)
            || live.requires_stop
        {
            return Err(ProxyError::InvalidApiKey);
        }
        if let Some(expiry) = live.lease_expires_at {
            self.lease_expires_at = self.lease_expires_at.max(expiry);
        }
        if let Some(reservation) = reservation {
            self.held_model_keys.extend(reservation.keys());
            self.held_model_keys.sort_unstable();
            self.held_model_keys.dedup();
            self.model_reservations.push(reservation);
        }
        self.entries.insert(
            entry_id.clone(),
            AsrEntry {
                accepted: entry,
                model: Arc::clone(&target.model_entry),
                pk_id: target.pk_entry.id.clone(),
                token_keys,
            },
        );
        self.configured_asr = Some(Some(entry_id));
        if let Some(slot) = event.pointer_mut(&format!("{pointer}/model")) {
            *slot = Value::String(upstream_model);
        }
        serde_json::to_vec(&event)
            .map(Some)
            .map_err(|_| ProxyError::AccountUnavailable)
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
                }
            }
            Some("input_audio_buffer.committed") => {
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

    fn observe_asr_meter(&mut self, event: &Value) -> Result<(), ProxyError> {
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
            let entry = self
                .asr_entry(event)
                .ok_or(ProxyError::AccountUnavailable)?;
            self.state
                .limiter
                .add_tokens_post_stream_all(&entry.token_keys, input + output);
            let snapshot = self.state.snapshot.load();
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
            self.secondary_cost_usd += cost;
        }
        Ok(())
    }

    /// 相同供应商事实换 event_id 重投只观察一次；不以传输投递 ID 定义收费事实。
    pub(crate) async fn observe(&mut self, text: &str) -> Result<bool, ProxyError> {
        let Ok(event) = serde_json::from_str::<Value>(text) else {
            return Ok(true);
        };
        self.observe_configuration(&event)?;
        self.observe_asr_meter(&event)?;
        self.flush().await?;
        if event["type"].as_str() == Some("conversation.item.input_audio_transcription.completed") {
            let identity = event["item_id"]
                .as_str()
                .zip(event["content_index"].as_u64());
            let entry = self.asr_entry(&event);
            let Some(entry) = entry else {
                return self.unknown_usage(&event["usage"]).await;
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
                return self.unknown_usage(&event["usage"]).await;
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
            self.pending = Some(AppendLiveUsageRequest {
                operation_id: self.lease.operation_id,
                api_key_id: self.api_key_id,
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
                lease: Some(self.lease.clone()),
            });
            return self.flush().await;
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
            self.pending = Some(AppendLiveUsageRequest {
                operation_id: self.lease.operation_id,
                api_key_id: self.api_key_id,
                event_id: format!("gateway-observation:{}", Uuid::new_v4()),
                response_id: None,
                usage: Vec::new(),
                coverage: UsageCoverage::Unknown,
                terminal: false,
                raw_usage: Some(usage.clone()),
                lease: Some(self.lease.clone()),
            });
            return self.flush().await;
        }
        let (Some(id), Some(measured)) = (id, measured) else {
            return Ok(true);
        };
        let at = measured.occurred_at;
        self.pending = Some(AppendLiveUsageRequest {
            operation_id: self.lease.operation_id,
            api_key_id: self.api_key_id,
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
            lease: Some(self.lease.clone()),
        });
        self.flush().await
    }

    async fn unknown_usage(&mut self, usage: &Value) -> Result<bool, ProxyError> {
        if !usage.is_object() {
            return Ok(true);
        }
        self.pending = Some(AppendLiveUsageRequest {
            operation_id: self.lease.operation_id,
            api_key_id: self.api_key_id,
            event_id: format!("gateway-observation:{}", Uuid::new_v4()),
            response_id: None,
            usage: Vec::new(),
            coverage: UsageCoverage::Unknown,
            terminal: false,
            raw_usage: Some(usage.clone()),
            lease: Some(self.lease.clone()),
        });
        self.flush().await
    }

    /// 未知结果只用原稳定事实重试，不能退回会话汇总收费。
    async fn flush(&mut self) -> Result<bool, ProxyError> {
        let Some(request) = self.pending.as_ref() else {
            return Ok(true);
        };
        let reply = self
            .registry
            .append_live_usage(request.clone())
            .await
            .map_err(proxy_error)?;
        if reply.operation_id != Some(self.lease.operation_id) {
            return Err(ProxyError::AccountUnavailable);
        }
        let appended = reply.appended.ok_or(ProxyError::AccountUnavailable)?;
        self.pending = None;
        Ok(appended)
    }

    pub(crate) async fn finish(&mut self) -> Result<(), ProxyError> {
        self.flush().await?;
        // 普通 WS 无供应商会话累计终账；连接结束不能证明缺失尾量为零。
        self.registry
            .checkpoint_live_session(CheckpointLiveSessionRequest {
                lease: self.lease.clone(),
                terminal: true,
                coverage: UsageCoverage::Unknown,
                entries: Vec::new(),
            })
            .await
            .map_err(proxy_error)?;
        Ok(())
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

fn terminal_usage(usage: &Value, status: Option<&str>) -> Option<TerminalUsage> {
    let prompt_tokens = u32::try_from(usage.get("input_tokens")?.as_u64()?).ok()?;
    let completion_tokens = u32::try_from(usage.get("output_tokens")?.as_u64()?).ok()?;
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
