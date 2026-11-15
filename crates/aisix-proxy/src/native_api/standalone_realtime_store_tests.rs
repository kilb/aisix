//! 实时能力的真实 SQLite 合同；默认 Unavailable 只能证明入口缺失，不能冒充深层账本 RED。
use super::*;
use aisix_core::native_realtime::*;
use base64::Engine as _;

const CAPABILITY_HASH: &str = "a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4";

fn live_submission(f: &Fixture) -> BeginRequest {
    let mut submitted = f.begin("key-a", NativeObjectKind::RealtimeCall, false);
    submitted.operation = "realtime".into();
    submitted
}

async fn accepted_live(f: &Fixture, store: &StandaloneNativeStore) -> BeginRequest {
    let submitted = live_submission(f);
    store.begin(submitted.clone()).await.unwrap();
    submitted
}

async fn minted(f: &Fixture) -> (MintCredentialRequest, NativeCredential) {
    let submitted = accepted_live(f, &f.store).await;
    let request = MintCredentialRequest {
        operation_id: submitted.operation_id,
        api_key_id: submitted.api_key_id,
        token_hash: CAPABILITY_HASH.into(),
        purpose: RealtimePurpose::Realtime,
        expires_at: Utc::now() + chrono::Duration::minutes(10),
        defaults: json!({"type":"realtime","model":"test-model","tools":[{"type":"function","name":"typed_input","parameters":{"type":"object","properties":{"value":{"type":"string"},"token":{"type":"string"}}}}]}),
    };
    let reply = f.store.mint_credential(request.clone()).await.unwrap();
    let wire = serde_json::to_string(&reply).unwrap();
    assert!(!wire.contains(CAPABILITY_HASH));
    assert!(!wire.contains("test-only-secret"));
    let credential = reply.credential.expect("explicit persisted credential");
    assert!(credential.usable_at(RealtimePurpose::Realtime, Utc::now()));
    assert_eq!(credential.api_key_id, submitted.api_key_id);
    assert_eq!(credential.defaults, request.defaults);
    (request, credential)
}

async fn created(
    f: &Fixture,
    store: &StandaloneNativeStore,
    credential_id: Option<Uuid>,
) -> (BeginRequest, LiveSessionRecord) {
    let submitted = accepted_live(f, store).await;
    let request = CreateLiveSessionRequest {
        operation_id: submitted.operation_id,
        api_key_id: submitted.api_key_id,
        purpose: RealtimePurpose::Realtime,
        transport: RealtimeTransport::WebSocket,
        credential_id,
        provider_session_id: Some(format!("sess.raw.{}", Uuid::new_v4())),
        deadline_at: Utc::now() + chrono::Duration::hours(3),
    };
    let reply = store.create_live_session(request.clone()).await.unwrap();
    let live = reply.live_session.expect("explicit durable live session");
    assert_eq!(live.operation_id, submitted.operation_id);
    assert_eq!(live.api_key_id, submitted.api_key_id);
    assert_eq!(live.state, "live");
    assert!(live
        .lease
        .as_ref()
        .is_some_and(|lease| lease.generation > 0));
    assert_eq!(live.deadline_at, Some(request.deadline_at));
    (submitted, live)
}

fn observation(
    entry: &str,
    upstream: &str,
    prompt: u32,
    completion: u32,
) -> aisix_core::native_metering::ObservedUsage {
    let mut usage = terminal_usage(entry);
    usage.prompt_tokens = prompt;
    usage.completion_tokens = completion;
    usage.cached_prompt_tokens = 0;
    usage.total_tokens = prompt + completion;
    serde_json::from_value(json!({"entry_id":entry,"upstream_model":upstream,"occurred_at":usage.occurred_at,"meter":{"kind":"tokens","usage":usage}})).unwrap()
}

fn append_request(
    submitted: &BeginRequest,
    live: &LiveSessionRecord,
    event: &str,
    response: Option<&str>,
) -> AppendLiveUsageRequest {
    AppendLiveUsageRequest {
        operation_id: submitted.operation_id,
        api_key_id: submitted.api_key_id,
        event_id: event.into(),
        response_id: response.map(str::to_owned),
        usage: vec![observation("", "test-model", 7, 3)],
        coverage: UsageCoverage::Complete,
        terminal: false,
        raw_usage: None,
        lease: live.lease.clone(),
    }
}

fn checkpoint(live: &LiveSessionRecord, terminal: bool) -> CheckpointLiveSessionRequest {
    CheckpointLiveSessionRequest {
        lease: live.lease.clone().unwrap(),
        terminal,
        coverage: UsageCoverage::Complete,
        entries: vec![],
    }
}

#[tokio::test]
async fn capability_survives_reopen_and_concurrent_redemption_for_independent_sessions() {
    let f = Fixture::new().await;
    let (request, credential) = minted(&f).await;
    let replay = f.store.mint_credential(request.clone()).await.unwrap();
    assert_eq!(
        replay.credential.unwrap().credential_id,
        credential.credential_id
    );
    let mut changed = request;
    changed.defaults = json!({"model":"another-default"});
    assert_eq!(
        f.store.mint_credential(changed).await.unwrap_err(),
        Error::Conflict
    );
    let first = f.reopen().await;
    let second = f.reopen().await;
    let redeem = RedeemCredentialRequest {
        token_hash: CAPABILITY_HASH.into(),
        purpose: RealtimePurpose::Realtime,
        client_ip: Some("127.0.0.1".into()),
    };
    let (one, two) = tokio::join!(
        first.redeem_credential(redeem.clone()),
        second.redeem_credential(redeem.clone())
    );
    for reply in [one, two] {
        assert_eq!(
            reply.unwrap().credential.unwrap().credential_id,
            credential.credential_id
        );
    }
    let (a, _) = created(&f, &first, Some(credential.credential_id)).await;
    let (b, _) = created(&f, &second, Some(credential.credential_id)).await;
    assert_ne!(a.operation_id, b.operation_id);
    let mut wrong = redeem.clone();
    wrong.purpose = RealtimePurpose::Translation;
    assert_eq!(
        first.redeem_credential(wrong).await.unwrap_err(),
        Error::NotFound
    );
    let mut invalid_ip = redeem;
    invalid_ip.client_ip = Some("not-an-ip".into());
    assert_eq!(
        first.redeem_credential(invalid_ip).await.unwrap_err(),
        Error::NotFound
    );
}

#[tokio::test]
async fn capability_rechecks_key_generation_member_and_disabled_state() {
    let f = Fixture::new().await;
    minted(&f).await;
    let redeem = RedeemCredentialRequest {
        token_hash: CAPABILITY_HASH.into(),
        purpose: RealtimePurpose::Realtime,
        client_ip: None,
    };
    for change in ["disabled", "member", "team", "key_hash", "removed"] {
        let snapshot = (*f.snapshot.load()).clone();
        let original = (*snapshot.apikeys.get_by_id("key-a").unwrap().value).clone();
        let mut changed = original.clone();
        match change {
            "disabled" => changed.disabled = true,
            "member" => changed.user_id = Some("user-b".into()),
            "team" => changed.team_id = Some("team-b".into()),
            "key_hash" => changed.key_hash = "rotated-hash".into(),
            "removed" => {
                snapshot.apikeys.remove("key-a");
            }
            _ => unreachable!(),
        }
        if change != "removed" {
            snapshot
                .apikeys
                .insert(ResourceEntry::new("key-a", changed, 2));
        }
        f.snapshot.store(snapshot);
        assert_eq!(
            f.store.redeem_credential(redeem.clone()).await.unwrap_err(),
            Error::NotFound,
            "{change}"
        );
        let restored = (*f.snapshot.load()).clone();
        restored
            .apikeys
            .insert(ResourceEntry::new("key-a", original, 3));
        f.snapshot.store(restored);
        assert!(f
            .store
            .redeem_credential(redeem.clone())
            .await
            .unwrap()
            .credential
            .is_some());
    }
}

#[tokio::test]
async fn withdrawn_default_model_does_not_turn_the_capability_into_a_permanent_model_grant() {
    let f = Fixture::new().await;
    let (_, credential) = minted(&f).await;
    let prior = accepted_live(&f, &f.store).await;
    let snapshot = (*f.snapshot.load()).clone();
    let mut key = (*snapshot.apikeys.get_by_id("key-a").unwrap().value).clone();
    key.allowed_models = vec!["model-b".into()];
    key.allowed_model_ids = None;
    snapshot.apikeys.insert(ResourceEntry::new("key-a", key, 2));
    f.snapshot.store(snapshot);
    assert_eq!(
        f.store
            .redeem_credential(RedeemCredentialRequest {
                token_hash: CAPABILITY_HASH.into(),
                purpose: RealtimePurpose::Realtime,
                client_ip: None
            })
            .await
            .unwrap()
            .credential
            .unwrap()
            .credential_id,
        credential.credential_id
    );
    assert_eq!(
        f.store
            .create_live_session(CreateLiveSessionRequest {
                operation_id: prior.operation_id,
                api_key_id: prior.api_key_id,
                purpose: RealtimePurpose::Realtime,
                transport: RealtimeTransport::WebSocket,
                credential_id: None,
                provider_session_id: None,
                deadline_at: Utc::now() + chrono::Duration::hours(1),
            })
            .await
            .unwrap_err(),
        Error::NotFound,
        "an old begin does not authorize a new connection after model revocation"
    );
    let forbidden = live_submission(&f);
    assert_eq!(f.store.begin(forbidden).await.unwrap_err(), Error::NotFound);
    let mut allowed = live_submission(&f);
    allowed.origin.model_id = stable_resource_id("models", "model-b");
    allowed.requested_model_id = allowed.origin.model_id;
    allowed.requested_model = "model-b".into();
    f.store.begin(allowed.clone()).await.unwrap();
    let live = f
        .store
        .create_live_session(CreateLiveSessionRequest {
            operation_id: allowed.operation_id,
            api_key_id: allowed.api_key_id,
            purpose: RealtimePurpose::Realtime,
            transport: RealtimeTransport::WebSocket,
            credential_id: Some(credential.credential_id),
            provider_session_id: Some("sess.override".into()),
            deadline_at: Utc::now() + chrono::Duration::hours(2),
        })
        .await
        .unwrap()
        .live_session
        .unwrap();
    assert_eq!(live.operation_id, allowed.operation_id);
}

#[tokio::test]
async fn live_journal_deduplicates_event_and_response_without_discarding_revoked_tail() {
    let f = Fixture::new().await;
    let (submitted, live) = created(&f, &f.store, None).await;
    let request = append_request(&submitted, &live, "response.done.1", Some("response.raw.1"));
    assert_eq!(
        f.store
            .append_live_usage(request.clone())
            .await
            .unwrap()
            .appended,
        Some(true)
    );
    let snapshot = (*f.snapshot.load()).clone();
    snapshot.apikeys.remove("key-a");
    f.snapshot.store(snapshot);
    assert_eq!(
        f.store
            .append_live_usage(request.clone())
            .await
            .unwrap()
            .appended,
        Some(false)
    );
    let mut duplicate = request.clone();
    duplicate.event_id = "redelivery.1".into();
    assert_eq!(
        f.store.append_live_usage(duplicate).await.unwrap().appended,
        Some(false)
    );
    let mut conflict = request.clone();
    conflict.usage = vec![observation("", "test-model", 999, 3)];
    assert_eq!(
        f.store.append_live_usage(conflict).await.unwrap_err(),
        Error::Conflict
    );
    let heartbeat = f
        .store
        .checkpoint_live_session(checkpoint(&live, false))
        .await
        .unwrap()
        .live_session
        .unwrap();
    assert!(heartbeat.requires_stop);
    let mut tail = append_request(&submitted, &live, "tail.done.2", Some("response.raw.2"));
    tail.terminal = true;
    let closed = f.store.append_live_usage(tail.clone()).await.unwrap();
    assert_eq!(closed.appended, Some(true));
    assert_eq!(closed.live_session.as_ref().unwrap().state, "closed");
    assert_eq!(
        f.store.append_live_usage(tail).await.unwrap().appended,
        Some(false)
    );
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT usage FROM native_live_journal WHERE operation_id=? ORDER BY event_id",
    )
    .bind(submitted.operation_id.to_string())
    .fetch_all(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        let usage: Vec<aisix_core::native_metering::ObservedUsage> = decode(&row).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].upstream_model, "test-model");
    }
    f.store.close().await;
    let reopened = f.reopen().await;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_live_journal WHERE operation_id=?")
            .bind(submitted.operation_id.to_string())
            .fetch_one(&reopened.pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn a_reported_zero_and_a_missing_terminal_observation_have_distinct_durable_coverage() {
    let f = Fixture::new().await;
    let (missing, missing_live) = created(&f, &f.store, None).await;
    let closed = f
        .store
        .checkpoint_live_session(checkpoint(&missing_live, true))
        .await
        .unwrap()
        .live_session
        .unwrap();
    assert_eq!(closed.coverage, UsageCoverage::Unknown);
    assert_eq!(closed.accounting_state, "awaiting_reconciliation");
    let (zero, zero_live) = created(&f, &f.store, None).await;
    let mut known = append_request(&zero, &zero_live, "reported.zero", None);
    known.usage = vec![observation("", "test-model", 0, 0)];
    known.terminal = true;
    let closed = f
        .store
        .append_live_usage(known)
        .await
        .unwrap()
        .live_session
        .unwrap();
    assert_eq!(closed.coverage, UsageCoverage::Complete);
    assert_ne!(closed.accounting_state, "awaiting_reconciliation");
    let missing_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_live_journal WHERE operation_id=?")
            .bind(missing.operation_id.to_string())
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(missing_rows, 0);
    let wire: String =
        sqlx::query_scalar("SELECT usage FROM native_live_journal WHERE operation_id=?")
            .bind(zero.operation_id.to_string())
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    let rows: Vec<aisix_core::native_metering::ObservedUsage> = decode(&wire).unwrap();
    assert!(
        matches!(&rows[0].meter,ObservedMeter::Tokens {usage} if usage.prompt_tokens==0 && usage.completion_tokens==0)
    );
}

#[tokio::test]
async fn a_new_process_claims_only_expired_sessions_for_stop_and_fences_the_old_observer() {
    let f = Fixture::new().await;
    let (submitted, live) = created(&f, &f.store, None).await;
    let recovery = f.reopen().await;
    assert!(recovery
        .claim_live_sessions(ClaimLiveSessionsRequest { limit: 10 })
        .await
        .unwrap()
        .live_sessions
        .is_empty());
    sqlx::query("UPDATE native_live_sessions SET lease_expires_at=0 WHERE operation_id=?")
        .bind(submitted.operation_id.to_string())
        .execute(&f.store.pool)
        .await
        .unwrap();
    f.snapshot.store(AisixSnapshot::new());
    let claimed = recovery
        .claim_live_sessions(ClaimLiveSessionsRequest { limit: 10 })
        .await
        .unwrap()
        .live_sessions;
    assert_eq!(claimed.len(), 1);
    assert!(claimed[0].requires_stop);
    assert_eq!(claimed[0].coverage, UsageCoverage::Unknown);
    assert_eq!(claimed[0].accounting_state, "awaiting_reconciliation");
    let accepted = claimed[0]
        .accepted
        .as_ref()
        .expect("recovery retains exact accepted source, without provider secret");
    assert_eq!(
        serde_json::to_value(accepted).unwrap(),
        serde_json::to_value(&submitted).unwrap()
    );
    assert!(!serde_json::to_string(&claimed[0])
        .unwrap()
        .contains("test-only-secret"));
    assert!(
        claimed[0].lease.as_ref().unwrap().generation > live.lease.as_ref().unwrap().generation
    );
    assert_eq!(
        f.store
            .append_live_usage(append_request(&submitted, &live, "stale.tail", None))
            .await
            .unwrap_err(),
        Error::NotFound
    );
    let mut tail = append_request(&submitted, &claimed[0], "recovered.tail", None);
    tail.terminal = true;
    assert_eq!(
        recovery.append_live_usage(tail).await.unwrap().appended,
        Some(true)
    );
    assert!(recovery
        .claim_live_sessions(ClaimLiveSessionsRequest { limit: 10 })
        .await
        .unwrap()
        .live_sessions
        .is_empty());
}

async fn asr_revocation(reuse_entry: bool) {
    let f = Fixture::new().await;
    let snapshot = (*f.snapshot.load()).clone();
    let mut model = (*snapshot.models.get_by_id("model-b").unwrap().value).clone();
    model.model_name = Some("asr-model".into());
    snapshot
        .models
        .insert(ResourceEntry::new("model-b", model, 2));
    f.snapshot.store(snapshot);
    let (submitted, live) = created(&f, &f.store, None).await;
    let entry = NativeEntry {
        entry_id: "asr".into(),
        endpoint: "/v1/audio/transcriptions".into(),
        requested_model_id: stable_resource_id("models", "model-b"),
        requested_model: "model-b".into(),
        requested_service_tier: None,
        model_id: stable_resource_id("models", "model-b"),
        upstream_model: "asr-model".into(),
        upstream_provider: "openai".into(),
        upstream_input_text: None,
    };
    let mut accept = checkpoint(&live, false);
    accept.entries = vec![entry.clone()];
    f.store.checkpoint_live_session(accept).await.unwrap();
    let snapshot = (*f.snapshot.load()).clone();
    let mut key = (*snapshot.apikeys.get_by_id("key-a").unwrap().value).clone();
    key.allowed_models = vec!["model-a".into()];
    key.allowed_model_ids = None;
    snapshot.apikeys.insert(ResourceEntry::new("key-a", key, 2));
    f.snapshot.store(snapshot);
    let mut heartbeat = checkpoint(&live, false);
    if reuse_entry {
        heartbeat.entries.push(entry);
    }
    match f.store.checkpoint_live_session(heartbeat).await {
        Ok(reply) => assert!(reply.live_session.unwrap().requires_stop),
        Err(Error::NotFound | Error::Conflict) => {}
        other => panic!("current ASR revocation must stop or deny: {other:?}"),
    }
    let mut tail = append_request(&submitted, &live, "asr.accepted.tail", None);
    tail.usage = vec![observation("asr", "asr-model", 9, 2)];
    tail.terminal = true;
    assert_eq!(
        f.store
            .append_live_usage(tail.clone())
            .await
            .unwrap()
            .appended,
        Some(true)
    );
    assert_eq!(
        f.store.append_live_usage(tail).await.unwrap().appended,
        Some(false)
    );
    let journal: String =
        sqlx::query_scalar("SELECT usage FROM native_live_journal WHERE operation_id=?")
            .bind(submitted.operation_id.to_string())
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    let usage: Vec<aisix_core::native_metering::ObservedUsage> = decode(&journal).unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].entry_id, "asr");
    assert_eq!(usage[0].upstream_model, "asr-model");
    assert!(
        matches!(&usage[0].meter,ObservedMeter::Tokens {usage} if usage.prompt_tokens==9 && usage.completion_tokens==2)
    );
}

#[tokio::test]
async fn heartbeat_rechecks_every_accepted_asr_model_while_retaining_paid_tail() {
    asr_revocation(false).await;
}

#[tokio::test]
async fn identical_asr_entry_reuse_rechecks_current_authorization_and_preserves_history() {
    asr_revocation(true).await;
}

fn sip_delivery(
    submitted: &BeginRequest,
    webhook_id: &str,
    event: &str,
    call: &str,
) -> SignedSipIntakeRequest {
    let raw_body=json!({"id":event,"type":"realtime.call.incoming","data":{"call_id":call,"sip_headers":[{"name":"X-Tenant","value":"untrusted-other-tenant"}]}}).to_string();
    let webhook_timestamp = Utc::now().timestamp().to_string();
    let signed = format!("{webhook_id}.{webhook_timestamp}.{raw_body}");
    let signature = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[41u8; 32]),
        signed.as_bytes(),
    );
    SignedSipIntakeRequest {
        operation_id: submitted.operation_id,
        route_id: "trusted-sip".into(),
        webhook_id: webhook_id.into(),
        webhook_timestamp,
        webhook_signature: format!(
            "v1,{}",
            base64::engine::general_purpose::STANDARD.encode(signature.as_ref())
        ),
        raw_body,
        deadline_at: Utc::now() + chrono::Duration::hours(1),
    }
}

#[tokio::test]
async fn signed_sip_intake_uses_only_the_configured_owner_and_deduplicates_redelivery() {
    let f = Fixture::new().await;
    let snapshot = (*f.snapshot.load()).clone();
    let mut provider = serde_json::to_value(
        &*snapshot
            .provider_keys
            .get_by_id("provider-a")
            .unwrap()
            .value,
    )
    .unwrap();
    provider["request"] =
        json!({"default_headers":{"OpenAI-Project":"proj_standalone_realtime_fixture"}});
    provider["realtime_sip"] = json!({"route_id":"trusted-sip","webhook_secret":format!("whsec_{}",base64::engine::general_purpose::STANDARD.encode([41u8;32])),"api_key_id":"key-a","model_id":"model-a"});
    snapshot.provider_keys.insert(ResourceEntry::new(
        "provider-a",
        serde_json::from_value(provider).unwrap(),
        2,
    ));
    f.snapshot.store(snapshot);
    let submitted = accepted_live(&f, &f.store).await;
    let request = sip_delivery(
        &submitted,
        "delivery.one",
        "provider.event.one",
        "call.raw.keep",
    );
    let mut bad = request.clone();
    bad.raw_body = bad.raw_body.replace("call.raw.keep", "call.tampered");
    assert_eq!(
        f.store.signed_sip_intake(bad).await.unwrap_err(),
        Error::NotFound
    );
    let fresh = f.store.signed_sip_intake(request.clone()).await.unwrap();
    assert_eq!(fresh.appended, Some(true));
    assert_eq!(fresh.record.as_ref().unwrap().raw_id, "call.raw.keep");
    assert_eq!(
        fresh.record.as_ref().unwrap().api_key_id,
        Some(stable_resource_id("apikeys", "key-a"))
    );
    assert_eq!(
        fresh.live_session.as_ref().unwrap().transport,
        RealtimeTransport::Sip
    );
    let next = accepted_live(&f, &f.store).await;
    let repeat = f
        .store
        .signed_sip_intake(sip_delivery(
            &next,
            "delivery.two",
            "provider.event.one",
            "call.raw.keep",
        ))
        .await
        .unwrap();
    assert_eq!(repeat.appended, Some(false));
    assert_eq!(repeat.operation_id, Some(submitted.operation_id));
    let mut wrong_route = request;
    wrong_route.route_id = "caller-chosen-owner".into();
    assert_eq!(
        f.store.signed_sip_intake(wrong_route).await.unwrap_err(),
        Error::NotFound
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_sip_intakes WHERE call_id='call.raw.keep'")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn raw_sip_ids_and_unsafe_credential_defaults_never_create_authority() {
    let f = Fixture::new().await;
    let submitted = accepted_live(&f, &f.store).await;
    assert_eq!(
        f.store
            .create_live_session(CreateLiveSessionRequest {
                operation_id: submitted.operation_id,
                api_key_id: submitted.api_key_id,
                purpose: RealtimePurpose::Realtime,
                transport: RealtimeTransport::Sip,
                credential_id: None,
                provider_session_id: Some("call.self.asserted".into()),
                deadline_at: Utc::now() + chrono::Duration::hours(1)
            })
            .await
            .unwrap_err(),
        Error::InvalidRequest
    );
    for defaults in [
        json!({"client_secret":{"value":"provider-secret"}}),
        json!({"session":{"client_secret":"provider-secret"}}),
    ] {
        assert_eq!(
            f.store
                .mint_credential(MintCredentialRequest {
                    operation_id: submitted.operation_id,
                    api_key_id: submitted.api_key_id,
                    token_hash: CAPABILITY_HASH.into(),
                    purpose: RealtimePurpose::Realtime,
                    expires_at: Utc::now() + chrono::Duration::minutes(5),
                    defaults
                })
                .await
                .unwrap_err(),
            Error::InvalidRequest
        );
    }
    for expires_at in [
        Utc::now() - chrono::Duration::seconds(1),
        Utc::now() + chrono::Duration::hours(3),
    ] {
        assert_eq!(
            f.store
                .mint_credential(MintCredentialRequest {
                    operation_id: submitted.operation_id,
                    api_key_id: submitted.api_key_id,
                    token_hash: CAPABILITY_HASH.into(),
                    purpose: RealtimePurpose::Realtime,
                    expires_at,
                    defaults: json!({}),
                })
                .await
                .unwrap_err(),
            Error::InvalidRequest
        );
    }
}

#[tokio::test]
async fn new_connection_rechecks_current_model_group_owner_and_provider_origin() {
    for change in ["model", "group", "owner", "origin"] {
        let f = Fixture::new().await;
        let submitted = accepted_live(&f, &f.store).await;
        let snapshot = (*f.snapshot.load()).clone();
        let mut key = (*snapshot.apikeys.get_by_id("key-a").unwrap().value).clone();
        match change {
            "model" => {
                key.allowed_models.clear();
                key.allowed_model_ids = None;
            }
            "group" => key.allowed_provider_groups = vec!["unavailable-group".into()],
            "owner" => key.user_id = Some("user-b".into()),
            "origin" => {
                let mut provider = (*snapshot
                    .provider_keys
                    .get_by_id("provider-a")
                    .unwrap()
                    .value)
                    .clone();
                provider.api_key = "rotated-provider-credential".into();
                snapshot
                    .provider_keys
                    .insert(ResourceEntry::new("provider-a", provider, 2));
            }
            _ => unreachable!(),
        }
        snapshot.apikeys.insert(ResourceEntry::new("key-a", key, 2));
        f.snapshot.store(snapshot);
        let request = CreateLiveSessionRequest {
            operation_id: submitted.operation_id,
            api_key_id: submitted.api_key_id,
            purpose: RealtimePurpose::Realtime,
            transport: RealtimeTransport::WebSocket,
            credential_id: None,
            provider_session_id: Some("sess.current-check".into()),
            deadline_at: Utc::now() + chrono::Duration::hours(1),
        };
        assert_eq!(
            f.store.create_live_session(request).await.unwrap_err(),
            Error::NotFound,
            "{change}"
        );
    }
}

fn configure_sip(f: &Fixture, key: [u8; 32]) {
    let snapshot = (*f.snapshot.load()).clone();
    let mut provider = serde_json::to_value(
        &*snapshot
            .provider_keys
            .get_by_id("provider-a")
            .unwrap()
            .value,
    )
    .unwrap();
    provider["request"] =
        json!({"default_headers":{"OpenAI-Project":"proj_standalone_realtime_fixture"}});
    provider["realtime_sip"] = json!({"route_id":"trusted-sip","webhook_secret":format!("whsec_{}",base64::engine::general_purpose::STANDARD.encode(key)),"api_key_id":"key-a","model_id":"model-a"});
    snapshot.provider_keys.insert(ResourceEntry::new(
        "provider-a",
        serde_json::from_value(provider).unwrap(),
        2,
    ));
    f.snapshot.store(snapshot);
}

#[tokio::test]
async fn atomic_sip_intake_creates_no_orphan_operation_for_bad_bytes_or_duplicate_delivery() {
    let f = Fixture::new().await;
    configure_sip(&f, [41; 32]);
    let submitted = live_submission(&f);
    let delivery = sip_delivery(&submitted, "atomic.one", "atomic.event", "call.atomic");
    let mut bad = delivery.clone();
    bad.raw_body = bad.raw_body.replace("call.atomic", "call.tampered");
    assert_eq!(
        f.store.signed_sip_intake(bad).await.unwrap_err(),
        Error::NotFound
    );
    let operations: i64 = sqlx::query_scalar("SELECT count(*) FROM native_operations")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(operations, 0);
    let accepted = f.store.signed_sip_intake(delivery).await.unwrap();
    assert_eq!(accepted.operation_id, Some(submitted.operation_id));
    assert_eq!(accepted.appended, Some(true));
    assert_eq!(accepted.record.as_ref().unwrap().raw_id, "call.atomic");
    assert_eq!(
        accepted.record.as_ref().unwrap().api_key_id,
        Some(stable_resource_id("apikeys", "key-a"))
    );
    let fresh = live_submission(&f);
    let duplicate = f
        .store
        .signed_sip_intake(sip_delivery(
            &fresh,
            "atomic.two",
            "atomic.event",
            "call.atomic",
        ))
        .await
        .unwrap();
    assert_eq!(duplicate.operation_id, Some(submitted.operation_id));
    assert_eq!(duplicate.appended, Some(false));
    let counts: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM native_operations),(SELECT count(*) FROM native_live_sessions),(SELECT count(*) FROM native_objects),(SELECT count(*) FROM native_sip_intakes)")
        .fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(counts, (1, 1, 1, 1));
}

#[tokio::test]
async fn an_old_signing_secret_cannot_leave_an_operation_after_current_binding_rotation() {
    let f = Fixture::new().await;
    configure_sip(&f, [41; 32]);
    let submitted = live_submission(&f);
    let stale = sip_delivery(&submitted, "rotated.one", "rotated.event", "call.rotated");
    configure_sip(&f, [42; 32]);
    assert_eq!(
        f.store.signed_sip_intake(stale).await.unwrap_err(),
        Error::NotFound
    );
    let counts: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM native_operations),(SELECT count(*) FROM native_live_sessions),(SELECT count(*) FROM native_objects),(SELECT count(*) FROM native_sip_intakes)")
        .fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0, 0));
}

#[tokio::test]
async fn a_partial_terminal_report_never_becomes_complete_accounting() {
    let f = Fixture::new().await;
    let (submitted, live) = created(&f, &f.store, None).await;
    let mut partial = append_request(&submitted, &live, "partial.terminal", None);
    partial.coverage = UsageCoverage::Partial;
    partial.terminal = true;
    let closed = f
        .store
        .append_live_usage(partial)
        .await
        .unwrap()
        .live_session
        .unwrap();
    assert_eq!(closed.state, "closed");
    assert_eq!(closed.coverage, UsageCoverage::Partial);
    assert_eq!(closed.accounting_state, "awaiting_reconciliation");
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_live_journal WHERE operation_id=?")
            .bind(submitted.operation_id.to_string())
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);
}

mod expiry_tail {
    use super::*;
    include!("standalone_realtime_expiry_tests.rs");
    include!("standalone_realtime_terminal_close_tests.rs");
}
