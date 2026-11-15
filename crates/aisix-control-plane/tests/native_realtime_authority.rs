//! 真实 CP/Postgres 的实时授权与持久账本合同，不连接或替代供应商。

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use base64::Engine as _;
use chrono::{Duration, Utc};
use ring::hmac;
use serde_json::{json, Value};
use support::world::{seed_key, seed_tenant};
use support::{Bundle, DpClient, OperatorSession, Resp, TestCp};
use uuid::Uuid;

const PREFIX: &str = "/dp/v1/native-objects";
const BASE: &str = "https://realtime-authority.example/v1";
const PROVIDER_KEY: &str = "sk-realtime-fixture-not-a-provider-secret";
const PROJECT: &str = "proj_realtime_authority_fixture";
const TOKEN: &str = "a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4";

fn id(value: &Value, pointer: &str) -> Uuid {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(|| panic!("missing explicit UUID {pointer}: {value}"))
}

fn boundary(cp: &TestCp, provider: Uuid) -> String {
    let key = ring::digest::digest(
        &ring::digest::SHA256,
        cp.state().pki.platform_id_key_hex().as_bytes(),
    );
    let mut input = b"aisix-native-origin-v1\0".to_vec();
    let provider = provider.to_string();
    for field in [provider.as_str(), "openai", "openai", BASE, PROVIDER_KEY] {
        input.extend_from_slice(&(field.len() as u64).to_be_bytes());
        input.extend_from_slice(field.as_bytes());
    }
    input.extend_from_slice(&1_u64.to_be_bytes());
    for field in ["openai-project", PROJECT] {
        input.extend_from_slice(&(field.len() as u64).to_be_bytes());
        input.extend_from_slice(field.as_bytes());
    }
    format!(
        "v1:{}",
        hex::encode(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key.as_ref()), &input).as_ref())
    )
}

struct Fixture {
    cp: TestCp,
    admin: OperatorSession,
    dp: DpClient,
    bundle: Bundle,
    tenant: Uuid,
    member: Uuid,
    key: Uuid,
    model: Uuid,
    provider: Uuid,
    template: Value,
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let provider=cp.send("POST","/admin/resources/provider_keys",Some(admin.cookie()),Some(json!({"doc":{
        "display_name":"realtime-authority-account","provider":"openai","api_key":PROVIDER_KEY,
        "api_base":BASE,"groups":["shared"],"request":{"default_headers":{"OpenAI-Project":PROJECT}}
    }}))).await;
    assert_eq!(provider.status, 201, "{provider:?}");
    let provider = id(&provider.body, "/id");
    let model=cp.send("POST","/admin/resources/models",Some(admin.cookie()),Some(json!({"doc":{
        "display_name":"realtime-authority-model","provider":"openai","model_name":"gpt-realtime",
        "provider_key_id":provider
    }}))).await;
    assert_eq!(model.status, 201, "{model:?}");
    let model = id(&model.body, "/id");
    let tenant = seed_tenant(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[model]).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let key = Uuid::parse_str(&seed_key(&cp, &admin, tenant, Some(member)).await).unwrap();
    let dp_admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = cp
        .issue_bundle(&dp_admin, "realtime-authority-creator")
        .await;
    let dp = cp.dp_client(&bundle);
    let template = json!({
        "operation_id":Uuid::new_v4(),"api_key_id":key,"protocol":"openai","kind":"realtime_call",
        "origin":{"model_id":model,"provider_key_id":provider,"boundary":boundary(&cp,provider)},
        "requested_model_id":model,"requested_model":"realtime-authority-model","upstream_model":"gpt-realtime",
        "upstream_provider":"openai","operation":"realtime","request_id":"rt-contract-request",
        "asynchronous":false
    });
    Some(Fixture {
        cp,
        admin,
        dp,
        bundle,
        tenant,
        member,
        key,
        model,
        provider,
        template,
    })
}

async fn grant(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, models: &[Uuid]) {
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id=$1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .unwrap();
    let response = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            admin.cookie(),
            version,
            json!({"allowed_model_ids":models}),
        )
        .await;
    assert_eq!(response.status, 200, "{response:?}");
}

async fn post(dp: &DpClient, path: &str, body: Value) -> Resp {
    dp.send("POST", &format!("{PREFIX}/{path}"), Some(body))
        .await
}

async fn begin(dp: &DpClient, template: &Value) -> Value {
    let mut body = template.clone();
    body["operation_id"] = json!(Uuid::new_v4());
    let response = post(dp, "begin", body.clone()).await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.body["operation_id"], body["operation_id"]);
    body
}

async fn mint(f: &Fixture) -> (Value, Value) {
    let accepted = begin(&f.dp, &f.template).await;
    let body = json!({"operation_id":accepted["operation_id"],"api_key_id":f.key,"token_hash":TOKEN,
        "purpose":"realtime","expires_at":Utc::now()+Duration::minutes(10),
        "defaults":{"model":"gpt-realtime","type":"realtime"}});
    let response = post(&f.dp, "credentials/mint", body.clone()).await;
    assert_eq!(response.status, 200, "{response:?}");
    assert!(
        response.body.get("credential").is_some(),
        "a real persisted credential reply"
    );
    assert!(!response.body.to_string().contains(PROVIDER_KEY));
    assert!(!response.body.to_string().contains(TOKEN));
    (body, response.body["credential"].clone())
}

async fn create(dp: &DpClient, template: &Value, credential: Option<&Value>) -> (Value, Value) {
    let accepted = begin(dp, template).await;
    let body = json!({"operation_id":accepted["operation_id"],"api_key_id":accepted["api_key_id"],
        "purpose":"realtime","transport":"web_socket","credential_id":credential.map(|value|value["credential_id"].clone()),
        "provider_session_id":format!("sess_raw.{}",Uuid::new_v4()),
        "deadline_at":Utc::now()+Duration::hours(3)});
    let response = post(dp, "live/create", body).await;
    assert_eq!(response.status, 200, "{response:?}");
    let record = response
        .body
        .get("live_session")
        .expect("explicit live session")
        .clone();
    assert_eq!(record["operation_id"], accepted["operation_id"]);
    assert_eq!(record["state"], "live");
    assert!(record
        .pointer("/lease/generation")
        .and_then(Value::as_i64)
        .is_some_and(|generation| generation > 0));
    (accepted, record)
}

fn usage(accepted: &Value, session: &Value, event: &str, response: &str) -> Value {
    json!({"operation_id":accepted["operation_id"],"api_key_id":accepted["api_key_id"],
        "event_id":event,"response_id":response,"lease":session["lease"],"coverage":"complete","terminal":false,
        "usage":[{"entry_id":"","upstream_model":"gpt-realtime","occurred_at":Utc::now(),
            "meter":{"kind":"tokens","usage":{"entry_id":"","occurred_at":Utc::now(),"status_code":200,
                "prompt_tokens":10,"completion_tokens":3,"cached_prompt_tokens":2,"total_tokens":13}}}]})
}

// 删除 hash/current-principal 检查会让禁用、换代、用途不匹配的兑换错误地成功。
#[tokio::test]
async fn credential_is_durable_multi_session_and_rechecks_original_key_and_member() {
    let Some(f) = fixture().await else { return };
    let (_body, credential) = mint(&f).await;
    let second = f.cp.restart().await;
    let redeemer = second.issue_dp_client("realtime-new-gateway").await;
    let request = json!({"token_hash":TOKEN,"purpose":"realtime","client_ip":"127.0.0.1"});
    for _ in 0..2 {
        let redeemed = post(&redeemer, "credentials/redeem", request.clone()).await;
        assert_eq!(redeemed.status, 200, "{redeemed:?}");
        assert_eq!(
            redeemed.body["credential"]["credential_id"],
            credential["credential_id"]
        );
        assert_eq!(redeemed.body["credential"]["api_key_id"], json!(f.key));
    }
    let (first, _) = create(&redeemer, &f.template, Some(&credential)).await;
    let (next, _) = create(&redeemer, &f.template, Some(&credential)).await;
    assert_ne!(
        first["operation_id"], next["operation_id"],
        "one token admits independent sessions"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_live_sessions WHERE credential_id=$1")
            .bind(id(&credential, "/credential_id"))
            .fetch_one(second.db().pool())
            .await
            .unwrap();
    assert_eq!(count, 2);
    let wrong = post(
        &redeemer,
        "credentials/redeem",
        json!({"token_hash":TOKEN,"purpose":"translation"}),
    )
    .await;
    let missing=post(&redeemer,"credentials/redeem",json!({"token_hash":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","purpose":"realtime"})).await;
    assert_eq!(wrong.status, 404, "{wrong:?}");
    assert_eq!(
        wrong.body, missing.body,
        "unknown and wrong-purpose credentials disclose no identity"
    );
    for (deny, restore) in [
        (
            "UPDATE tenant_keys SET disabled=true WHERE id=$1",
            "UPDATE tenant_keys SET disabled=false WHERE id=$1",
        ),
        (
            "UPDATE tenant_keys SET operator_disabled=true WHERE id=$1",
            "UPDATE tenant_keys SET operator_disabled=false WHERE id=$1",
        ),
    ] {
        sqlx::query(deny)
            .bind(f.key)
            .execute(second.db().pool())
            .await
            .unwrap();
        let denied = post(&redeemer, "credentials/redeem", request.clone()).await;
        assert_eq!(denied.status, 404, "{denied:?}");
        sqlx::query(restore)
            .bind(f.key)
            .execute(second.db().pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE members SET disabled=true WHERE id=$1")
        .bind(f.member)
        .execute(second.db().pool())
        .await
        .unwrap();
    assert_eq!(
        post(&redeemer, "credentials/redeem", request.clone())
            .await
            .status,
        404
    );
    sqlx::query("UPDATE members SET disabled=false WHERE id=$1")
        .bind(f.member)
        .execute(second.db().pool())
        .await
        .unwrap();
    sqlx::query("UPDATE tenant_keys SET key_hash='sha256:rotated-original-key' WHERE id=$1")
        .bind(f.key)
        .execute(second.db().pool())
        .await
        .unwrap();
    assert_eq!(
        post(&redeemer, "credentials/redeem", request).await.status,
        404,
        "rotated key generation invalidates its minted capability"
    );
}

// 兑换不把发行时默认模型当永久授权；最终 override 仍必须经过当前 begin 检查。
#[tokio::test]
async fn withdrawing_default_model_does_not_block_an_authorized_override() {
    let Some(f) = fixture().await else { return };
    let (_body, credential) = mint(&f).await;
    let alternate=f.cp.send("POST","/admin/resources/models",Some(f.admin.cookie()),Some(json!({"doc":{
        "display_name":"realtime-alternate-model","provider":"openai","model_name":"gpt-realtime-mini",
        "provider_key_id":f.provider
    }}))).await;
    assert_eq!(alternate.status, 201, "{alternate:?}");
    let alternate = id(&alternate.body, "/id");
    grant(&f.cp, &f.admin, f.tenant, &[alternate]).await;
    let redeemed = post(
        &f.dp,
        "credentials/redeem",
        json!({"token_hash":TOKEN,"purpose":"realtime"}),
    )
    .await;
    assert_eq!(
        redeemed.status, 200,
        "a valid override must remain reachable: {redeemed:?}"
    );
    let mut default = f.template.clone();
    default["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(
        post(&f.dp, "begin", default).await.status,
        404,
        "the withdrawn default no longer admits inference"
    );
    let mut override_model = f.template.clone();
    override_model["origin"]["model_id"] = json!(alternate);
    override_model["requested_model_id"] = json!(alternate);
    override_model["requested_model"] = json!("realtime-alternate-model");
    override_model["upstream_model"] = json!("gpt-realtime-mini");
    create(&f.dp, &override_model, Some(&credential)).await;
}

// 事件 ID 与 response ID 任一去重失效都会增加 outbox 计数；不是只看成功响应。
#[tokio::test]
async fn each_received_usage_is_durable_once_before_close_even_after_key_revocation() {
    let Some(f) = fixture().await else { return };
    let (accepted, session) = create(&f.dp, &f.template, None).await;
    let mut body = usage(&accepted, &session, "event.real.1", "resp.raw.1");
    sqlx::query("UPDATE tenant_keys SET revoked_at=clock_timestamp() WHERE id=$1")
        .bind(f.key)
        .execute(f.cp.db().pool())
        .await
        .unwrap();
    let first = post(&f.dp, "live/append", body.clone()).await;
    assert_eq!(
        first.status, 200,
        "accepted observation survives creator revocation: {first:?}"
    );
    assert_eq!(first.body.get("appended"), Some(&json!(true)));
    let repeated = post(&f.dp, "live/append", body.clone()).await;
    assert_eq!(repeated.status, 200, "{repeated:?}");
    assert_eq!(repeated.body.get("appended"), Some(&json!(false)));
    body["event_id"] = json!("event.alias.1");
    let response_replay = post(&f.dp, "live/append", body.clone()).await;
    assert_eq!(response_replay.status, 200, "{response_replay:?}");
    assert_eq!(response_replay.body.get("appended"), Some(&json!(false)));
    body["usage"][0]["meter"]["usage"]["prompt_tokens"] = json!(11);
    assert_eq!(
        post(&f.dp, "live/append", body).await.status,
        409,
        "a duplicate identity cannot rewrite reported quantities"
    );
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM native_live_journal WHERE operation_id=$1),(SELECT count(*) FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.operation_id=$1)")
        .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(
        counts,
        (1, 1),
        "journal and billable outbox commit before any session close"
    );
    let outbox:Value=sqlx::query_scalar("SELECT event FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.operation_id=$1")
        .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(outbox.get("api_key_id"), Some(&json!(f.key)));
    assert_eq!(outbox.get("model_id"), Some(&json!(f.model)));
    assert_eq!(outbox.get("prompt_tokens"), Some(&json!(10)));
    assert_eq!(outbox.get("cached_prompt_tokens"), Some(&json!(2)));
    assert_eq!(outbox.get("total_tokens"), Some(&json!(13)));
    assert_eq!(outbox.get("inference_stage"), Some(&json!("terminal")));
    assert_eq!(
        outbox.get("native_operation_id"),
        accepted.get("operation_id")
    );
    let closed=post(&f.dp,"live/append",json!({"operation_id":accepted["operation_id"],"api_key_id":f.key,
        "event_id":"session.closed.missing_tail","lease":session["lease"],"coverage":"unknown","terminal":true,"usage":[]})).await;
    assert_eq!(closed.status, 200, "{closed:?}");
    assert_eq!(closed.body["live_session"]["state"], "closed");
    assert_eq!(closed.body["live_session"]["coverage"], "unknown");
    assert_eq!(
        closed.body["live_session"]["accounting_state"],
        "awaiting_reconciliation"
    );
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.operation_id=$1")
        .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(count, 1, "missing tail never creates a zero billable event");
}

// 去掉租约 generation 或 SKIP LOCKED，重启后的双 worker 就能重复占用旧会话。
#[tokio::test]
async fn restart_recovery_claims_only_stop_and_reconciliation_and_fences_stale_observers() {
    let Some(f) = fixture().await else { return };
    let (accepted, session) = create(&f.dp, &f.template, None).await;
    let deadline = session
        .get("deadline_at")
        .expect("fixed lifecycle deadline")
        .clone();
    sqlx::query("UPDATE native_live_sessions SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE operation_id=$1")
        .bind(id(&accepted,"/operation_id")).execute(f.cp.db().pool()).await.unwrap();
    let second = f.cp.restart().await;
    let recovery = second.issue_dp_client("realtime-recovery-a").await;
    let other = second.issue_dp_client("realtime-recovery-b").await;
    let (a, b) = tokio::join!(
        post(&recovery, "live/claim", json!({"limit":10})),
        post(&other, "live/claim", json!({"limit":10}))
    );
    assert_eq!(a.status, 200, "{a:?}");
    assert_eq!(b.status, 200, "{b:?}");
    let a_sessions = a.body["live_sessions"]
        .as_array()
        .expect("explicit collection");
    let b_sessions = b.body["live_sessions"]
        .as_array()
        .expect("explicit collection");
    assert_eq!(
        a_sessions.len() + b_sessions.len(),
        1,
        "one lease owner after CP restart"
    );
    let (worker, recovered) = if a_sessions.is_empty() {
        (&other, &b_sessions[0])
    } else {
        (&recovery, &a_sessions[0])
    };
    assert_eq!(recovered["operation_id"], accepted["operation_id"]);
    assert_eq!(
        recovered["requires_stop"], true,
        "recovery cannot repeat an unknown live dispatch"
    );
    assert_eq!(recovered["state"], "requires_stop");
    assert_eq!(recovered["coverage"], "unknown");
    assert_eq!(
        recovered["deadline_at"], deadline,
        "lease renewal cannot extend lifecycle deadline"
    );
    assert!(
        recovered["lease"]["generation"].as_i64().unwrap()
            > session["lease"]["generation"].as_i64().unwrap()
    );
    let stale = post(
        &f.dp,
        "live/append",
        usage(&accepted, &session, "stale.event", "stale.response"),
    )
    .await;
    assert_eq!(
        stale.status, 404,
        "stale gateway cannot keep appending after recovery ownership changes"
    );
    let mut tail = usage(&accepted, recovered, "recovered.tail", "recovered.response");
    tail["coverage"] = json!("partial");
    let appended = post(worker, "live/append", tail).await;
    assert_eq!(
        appended.status, 200,
        "a recovery observer can preserve a real tail: {appended:?}"
    );
    assert_eq!(
        appended.body["live_session"]["coverage"], "unknown",
        "a recovered tail cannot erase the missing interval"
    );
}

#[tokio::test]
async fn raw_sip_call_ids_and_provider_credentials_cannot_create_local_authority() {
    let Some(f) = fixture().await else { return };
    let accepted = begin(&f.dp, &f.template).await;
    let secret=post(&f.dp,"credentials/mint",json!({"operation_id":accepted["operation_id"],"api_key_id":f.key,
        "token_hash":TOKEN,"purpose":"realtime","expires_at":Utc::now()+Duration::minutes(10),
        "defaults":{"model":"gpt-realtime","client_secret":{"value":"ek_provider_must_never_persist"}}})).await;
    assert_eq!(
        secret.status, 422,
        "provider secret slots must fail before persistence: {secret:?}"
    );
    let forged = post(
        &f.dp,
        "live/create",
        json!({"operation_id":accepted["operation_id"],"api_key_id":f.key,
        "purpose":"realtime","transport":"sip","provider_session_id":"call_shared_account_raw",
        "deadline_at":Utc::now()+Duration::hours(1)}),
    )
    .await;
    assert_eq!(
        forged.status, 422,
        "only a verified provider-origin route can establish SIP ownership: {forged:?}"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM native_live_sessions WHERE operation_id=$1")
            .bind(id(&accepted, "/operation_id"))
            .fetch_one(f.cp.db().pool())
            .await
            .unwrap();
    assert_eq!(count, 0);
}

// 过期只阻止新 session，不会把已经建立的会话和尾量一起撤销。
#[tokio::test]
async fn capability_expiry_does_not_expire_an_already_admitted_session() {
    let Some(f) = fixture().await else { return };
    let accepted = begin(&f.dp, &f.template).await;
    let expires = Utc::now() + Duration::seconds(15);
    let minted=post(&f.dp,"credentials/mint",json!({"operation_id":accepted["operation_id"],"api_key_id":f.key,
        "token_hash":TOKEN,"purpose":"realtime","expires_at":expires,
        "defaults":{"model":"gpt-realtime","tools":[{"type":"function","name":"tool","parameters":{
            "type":"object","properties":{"value":{"type":"string"},"token":{"type":"string"},"secret":{"type":"string"}}
        }}]}})).await;
    assert_eq!(
        minted.status, 200,
        "legal tool JSON Schema names remain valid: {minted:?}"
    );
    let (session_accepted, session) =
        create(&f.dp, &f.template, Some(&minted.body["credential"])).await;
    tokio::time::sleep(std::time::Duration::from_secs(16)).await;
    let denied = post(
        &f.dp,
        "credentials/redeem",
        json!({"token_hash":TOKEN,"purpose":"realtime"}),
    )
    .await;
    assert_eq!(
        denied.status, 404,
        "an expired capability admits no new session: {denied:?}"
    );
    let appended = post(
        &f.dp,
        "live/append",
        usage(
            &session_accepted,
            &session,
            "after.expiry.event",
            "after.expiry.response",
        ),
    )
    .await;
    assert_eq!(
        appended.status, 200,
        "established session does not inherit temporary token expiry: {appended:?}"
    );
    assert_eq!(appended.body.get("appended"), Some(&json!(true)));
}

// ASR 的模型价和主实时模型不同；删除动态快照检查会让未知 ASR 量误记主模型。
#[tokio::test]
async fn dynamically_authorized_asr_entries_survive_revocation_without_using_the_main_model() {
    let Some(f) = fixture().await else { return };
    let asr=f.cp.send("POST","/admin/resources/models",Some(f.admin.cookie()),Some(json!({"doc":{
        "display_name":"realtime-asr-model","provider":"openai","model_name":"gpt-4o-mini-transcribe",
        "provider_key_id":f.provider
    }}))).await;
    assert_eq!(asr.status, 201, "{asr:?}");
    let asr = id(&asr.body, "/id");
    grant(&f.cp, &f.admin, f.tenant, &[f.model, asr]).await;
    let (accepted, session) = create(&f.dp, &f.template, None).await;
    let entry = json!({"entry_id":"asr.observed.1","endpoint":"/v1/audio/transcriptions",
        "requested_model_id":asr,"requested_model":"realtime-asr-model","model_id":asr,
        "upstream_model":"gpt-4o-mini-transcribe","upstream_provider":"openai"});
    let mut checkpoint = json!({"lease":session["lease"],"coverage":"partial","entries":[entry]});
    let registered = post(&f.dp, "live/checkpoint", checkpoint.clone()).await;
    assert_eq!(
        registered.status, 200,
        "ASR is authorized before its configuration is forwarded: {registered:?}"
    );
    checkpoint["entries"][0]["upstream_model"] = json!("a-different-model");
    assert_eq!(
        post(&f.dp, "live/checkpoint", checkpoint).await.status,
        409,
        "accepted entry IDs are immutable"
    );
    sqlx::query("UPDATE tenant_keys SET revoked_at=clock_timestamp() WHERE id=$1")
        .bind(f.key)
        .execute(f.cp.db().pool())
        .await
        .unwrap();
    let mut asr_usage = usage(&accepted, &session, "asr.event.1", "asr.item.raw:0");
    asr_usage["usage"][0]["entry_id"] = json!("asr.observed.1");
    asr_usage["usage"][0]["meter"]["usage"]["entry_id"] = json!("asr.observed.1");
    asr_usage["usage"][0]["upstream_model"] = json!("gpt-4o-mini-transcribe");
    let appended = post(&f.dp, "live/append", asr_usage).await;
    assert_eq!(appended.status, 200, "{appended:?}");
    let event:Value=sqlx::query_scalar("SELECT event FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.operation_id=$1")
        .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(event.get("model_id"), Some(&json!(asr)));
    assert_eq!(event.get("requested_model_id"), Some(&json!(asr)));
    assert_eq!(
        event.get("upstream_model"),
        Some(&json!("gpt-4o-mini-transcribe"))
    );
    assert_eq!(event.get("operation"), Some(&json!("transcription")));
    assert_ne!(event.get("model_id"), Some(&json!(f.model)));
    let mut unregistered = usage(
        &accepted,
        &session,
        "unknown.asr.event",
        "unknown.asr.item:0",
    );
    unregistered["usage"][0]["entry_id"] = json!("unregistered-asr");
    unregistered["usage"][0]["meter"]["usage"]["entry_id"] = json!("unregistered-asr");
    unregistered["usage"][0]["upstream_model"] = json!("gpt-4o-mini-transcribe");
    let unknown = post(&f.dp, "live/append", unregistered).await;
    assert_eq!(
        unknown.status, 200,
        "real observations are retained even if attribution is incomplete: {unknown:?}"
    );
    assert_eq!(
        unknown.body["live_session"]["accounting_state"],
        "awaiting_reconciliation"
    );
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.operation_id=$1")
        .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(
        count, 1,
        "unregistered ASR never falls through into a main-model bill"
    );
}

async fn assert_secondary_asr_revocation(f: &Fixture, reuse_entry: bool) {
    let asr=f.cp.send("POST","/admin/resources/models",Some(f.admin.cookie()),Some(json!({"doc":{
        "display_name":"realtime-revoked-asr-model","provider":"openai","model_name":"gpt-4o-mini-transcribe",
        "provider_key_id":f.provider
    }}))).await;
    assert_eq!(asr.status, 201, "{asr:?}");
    let asr = id(&asr.body, "/id");
    grant(&f.cp, &f.admin, f.tenant, &[f.model, asr]).await;
    support::world::price(&f.cp, asr).await;
    let (accepted, session) = create(&f.dp, &f.template, None).await;
    let entry = json!({"entry_id":"asr.before.revoke","endpoint":"/v1/audio/transcriptions",
        "requested_model_id":asr,"requested_model":"realtime-revoked-asr-model","model_id":asr,
        "upstream_model":"gpt-4o-mini-transcribe","upstream_provider":"openai"});
    let checkpoint = json!({"lease":session["lease"],"coverage":"partial","entries":[entry]});
    let registered = post(&f.dp, "live/checkpoint", checkpoint.clone()).await;
    assert_eq!(registered.status, 200, "{registered:?}");
    assert_eq!(registered.body["live_session"]["state"], "live");
    assert_eq!(
        registered.body["live_session"].get("requires_stop"),
        Some(&json!(false))
    );

    grant(&f.cp, &f.admin, f.tenant, &[f.model]).await;
    let grants: Vec<Uuid> = sqlx::query_scalar("SELECT allowed_model_ids FROM tenants WHERE id=$1")
        .bind(f.tenant)
        .fetch_one(f.cp.db().pool())
        .await
        .unwrap();
    assert_eq!(
        grants,
        vec![f.model],
        "only the secondary ASR grant was withdrawn"
    );
    begin(&f.dp, &f.template).await;

    let checkpoint = if reuse_entry {
        checkpoint
    } else {
        json!({"lease":session["lease"],"coverage":"partial"})
    };
    let checked = post(&f.dp, "live/checkpoint", checkpoint).await;
    if reuse_entry && checked.status == 404 {
        assert_eq!(
            checked.body.get("code"),
            Some(&json!("not_found")),
            "{checked:?}"
        );
    } else {
        assert_eq!(
            checked.status, 200,
            "an observer must receive an actionable verdict: {checked:?}"
        );
        assert_eq!(
            checked.body["live_session"].get("requires_stop"),
            Some(&json!(true)),
            "withdrawing ASR alone must prevent continued inference: {checked:?}"
        );
        assert_eq!(checked.body["live_session"]["state"], "requires_stop");
        assert_eq!(checked.body["live_session"]["coverage"], "unknown");
        assert_eq!(
            checked.body["live_session"]["deadline_at"],
            session["deadline_at"]
        );
    }

    let mut tail = usage(
        &accepted,
        &session,
        "revoked.asr.tail",
        "revoked.asr.item:0",
    );
    tail["coverage"] = json!("partial");
    tail["usage"][0]["entry_id"] = json!("asr.before.revoke");
    tail["usage"][0]["meter"]["usage"]["entry_id"] = json!("asr.before.revoke");
    tail["usage"][0]["upstream_model"] = json!("gpt-4o-mini-transcribe");
    let appended = post(&f.dp, "live/append", tail.clone()).await;
    assert_eq!(
        appended.status, 200,
        "accepted ASR work remains billable: {appended:?}"
    );
    assert_eq!(appended.body.get("appended"), Some(&json!(true)));
    for event in ["revoked.asr.tail", "revoked.asr.tail.redelivery"] {
        tail["event_id"] = json!(event);
        let repeated = post(&f.dp, "live/append", tail.clone()).await;
        assert_eq!(repeated.status, 200, "{repeated:?}");
        assert_eq!(repeated.body.get("appended"), Some(&json!(false)));
    }
    let closed = post(
        &f.dp,
        "live/checkpoint",
        json!({"lease":session["lease"],
        "coverage":"unknown","terminal":true}),
    )
    .await;
    assert_eq!(closed.status, 200, "cleanup remains reachable: {closed:?}");
    assert_eq!(closed.body["live_session"]["state"], "closed");
    assert_eq!(
        closed.body["live_session"]["accounting_state"],
        "awaiting_reconciliation"
    );
    let event: Value = sqlx::query_scalar("SELECT event FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.operation_id=$1")
        .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(event.get("model_id"), Some(&json!(asr)));
    assert_eq!(event.get("requested_model_id"), Some(&json!(asr)));
    assert_eq!(
        event.get("upstream_model"),
        Some(&json!("gpt-4o-mini-transcribe"))
    );
    assert_eq!(event.get("operation"), Some(&json!("transcription")));
    assert_eq!(event.get("prompt_tokens"), Some(&json!(10)));
    assert_eq!(event.get("cached_prompt_tokens"), Some(&json!(2)));
    assert_eq!(event.get("completion_tokens"), Some(&json!(3)));
    assert_eq!(event.get("total_tokens"), Some(&json!(13)));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let delivered: i64 = sqlx::query_scalar("SELECT count(*) FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id JOIN billing_ledger_cny ledger ON ledger.event_id=outbox.event_id::text WHERE journal.operation_id=$1")
                .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
            if delivered == 1 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("accepted ASR tail must reach the real billing ledger after model revocation");
    let counts: (i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM native_live_journal WHERE operation_id=$1), (SELECT count(*) FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.operation_id=$1), count(*), COALESCE(sum(-ledger.amount_micro_cny),0)::bigint FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id JOIN billing_ledger_cny ledger ON ledger.event_id=outbox.event_id::text WHERE journal.operation_id=$1")
        .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(counts, (1, 1, 1, 15), "one accepted ASR tail, one outbox event and one debit; the unpriced main model cannot substitute");
}

// 主模型仍可 begin；只撤 ASR 授权也必须停止会话，尾量仍按接受时的 ASR 身份入账一次。
#[tokio::test]
async fn heartbeat_after_asr_model_revocation_requires_stop_and_bills_tail_once() {
    let Some(f) = fixture().await else { return };
    assert_secondary_asr_revocation(&f, false).await;
}

// 相同 entry 的幂等重放不是继续推理的授权，不能绕过撤权检查。
#[tokio::test]
async fn reused_asr_entry_after_model_revocation_requires_stop_and_bills_tail_once() {
    let Some(f) = fixture().await else { return };
    assert_secondary_asr_revocation(&f, true).await;
}

// 删除 journal 永久身份，会在普通 usage 保留期结束后重新插入并重新计价。
#[tokio::test]
async fn live_outbox_uses_the_normal_intake_and_permanent_dedup_survives_usage_retention() {
    let Some(f) = fixture().await else { return };
    let (accepted, session) = create(&f.dp, &f.template, None).await;
    let body = usage(
        &accepted,
        &session,
        "retention.event.1",
        "retention.response.1",
    );
    let appended = post(&f.dp, "live/append", body.clone()).await;
    assert_eq!(appended.status, 200, "{appended:?}");
    let event_id:Uuid=sqlx::query_scalar("SELECT outbox.event_id FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.operation_id=$1")
        .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let delivered: bool = sqlx::query_scalar(
                "SELECT delivered_at IS NOT NULL FROM native_live_outbox WHERE event_id=$1",
            )
            .bind(event_id)
            .fetch_one(f.cp.db().pool())
            .await
            .unwrap();
            if delivered {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the real outbox worker must deliver before session close");
    let priced: String =
        sqlx::query_scalar("SELECT status FROM pricing_charges_cny WHERE event_id=$1")
            .bind(event_id.to_string())
            .fetch_one(f.cp.db().pool())
            .await
            .unwrap();
    assert_eq!(
        priced, "unpriced",
        "the fixture has no baseline, so no price is invented"
    );
    let stored: Value = sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id=$1")
        .bind(event_id.to_string())
        .fetch_one(f.cp.db().pool())
        .await
        .unwrap();
    assert_eq!(stored.get("prompt_tokens"), Some(&json!(10)));
    for statement in [
        "DELETE FROM pricing_charges_cny WHERE event_id=$1",
        "DELETE FROM usage_events WHERE event_id=$1",
    ] {
        sqlx::query(statement)
            .bind(event_id.to_string())
            .execute(f.cp.db().pool())
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM usage_batches WHERE batch_id=$1")
        .bind(event_id)
        .execute(f.cp.db().pool())
        .await
        .unwrap();
    let restarted = f.cp.restart().await;
    let rebound = restarted.dp_client(&f.bundle);
    let replay = post(&rebound, "live/append", body).await;
    assert_eq!(
        replay.status, 200,
        "durable duplicate result survives ordinary retention: {replay:?}"
    );
    assert_eq!(replay.body.get("appended"), Some(&json!(false)));
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.operation_id=$1")
        .bind(id(&accepted,"/operation_id")).fetch_one(restarted.db().pool()).await.unwrap();
    assert_eq!(count, 1);
    let redelivery: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_events WHERE event_id=$1")
        .bind(event_id.to_string())
        .fetch_one(restarted.db().pool())
        .await
        .unwrap();
    assert_eq!(
        redelivery, 0,
        "retention does not reopen a delivered outbox identity"
    );
}

async fn configure_sip(f: &Fixture, route: &str, secret: &str) {
    let resource =
        f.cp.send(
            "GET",
            &format!("/admin/resources/provider_keys/{}", f.provider),
            Some(f.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(resource.status, 200, "{resource:?}");
    let version = resource.body["version"]
        .as_i64()
        .expect("provider resource version");
    let mut doc = resource.body["doc"].clone();
    doc["realtime_sip"] =
        json!({"route_id":route,"webhook_secret":secret,"api_key_id":f.key,"model_id":f.model});
    let saved =
        f.cp.send_conditional(
            "PUT",
            &format!("/admin/resources/provider_keys/{}", f.provider),
            f.admin.cookie(),
            version,
            json!({"doc":doc}),
        )
        .await;
    assert_eq!(
        saved.status, 200,
        "a trusted SIP binding is reachable through normal resource validation: {saved:?}"
    );
}

fn sip_delivery(
    accepted: &Value,
    route: &str,
    secret: &str,
    webhook: &str,
    event: &str,
    call: &str,
) -> Value {
    let raw=json!({"id":event,"type":"realtime.call.incoming","created_at":Utc::now().timestamp(),
        "data":{"call_id":call,"sip_headers":[{"name":"To","value":"sip:untrusted-other-tenant@example.test"},
            {"name":"X-Tenant-ID","value":Uuid::new_v4()}]}}).to_string();
    let timestamp = Utc::now().timestamp().to_string();
    // 直接编码官方签名合同，避免使用产品的验签 helper 生成测试证明。
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(
            secret
                .strip_prefix("whsec_")
                .expect("official encoded secret"),
        )
        .unwrap();
    let signature = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &decoded),
        format!("{webhook}.{timestamp}.{raw}").as_bytes(),
    );
    json!({"operation_id":accepted["operation_id"],"route_id":route,"webhook_id":webhook,
        "webhook_timestamp":timestamp,"webhook_signature":format!("v1,{}",base64::engine::general_purpose::STANDARD.encode(signature.as_ref())),
        "raw_body":raw,"deadline_at":Utc::now()+Duration::hours(1)})
}

// 删除可信 route 绑定、签名或任一永久事件身份检查都会让这个合同失败。
#[tokio::test]
async fn signed_sip_intake_binds_only_the_configured_owner_and_deduplicates_independent_delivery_ids(
) {
    let Some(f) = fixture().await else { return };
    let secret = format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode([41_u8; 32])
    );
    configure_sip(&f, "trusted-route-a", &secret).await;
    let accepted = begin(&f.dp, &f.template).await;
    let delivery = sip_delivery(
        &accepted,
        "trusted-route-a",
        &secret,
        "delivery.1",
        "event.incoming.1",
        "call.raw.keep-001",
    );
    let mut tampered = delivery.clone();
    tampered["raw_body"] = json!(delivery["raw_body"]
        .as_str()
        .unwrap()
        .replace("call.raw.keep-001", "call.raw.forbidden"));
    let refused = post(&f.dp, "live/sip-intake", tampered).await;
    assert_eq!(
        refused.status, 404,
        "signed bytes cannot be substituted: {refused:?}"
    );
    let admitted = post(&f.dp, "live/sip-intake", delivery.clone()).await;
    assert_eq!(admitted.status, 200, "{admitted:?}");
    assert_eq!(admitted.body.get("appended"), Some(&json!(true)));
    assert_eq!(admitted.body["record"]["raw_id"], "call.raw.keep-001");
    assert_eq!(admitted.body["record"]["api_key_id"], json!(f.key));
    assert_eq!(admitted.body["live_session"]["transport"], "sip");
    let owner:(Uuid,Uuid,Option<Uuid>)=sqlx::query_as("SELECT tenant_id,api_key_id,member_id FROM native_objects WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(
        owner,
        (f.tenant, f.key, Some(f.member)),
        "untrusted SIP headers never supply owner identity"
    );
    let fresh = begin(&f.dp, &f.template).await;
    let redelivery = sip_delivery(
        &fresh,
        "trusted-route-a",
        &secret,
        "delivery.2",
        "event.incoming.1",
        "call.raw.keep-001",
    );
    let repeated = post(&f.dp, "live/sip-intake", redelivery).await;
    assert_eq!(
        repeated.status, 200,
        "a provider event may have another delivery ID: {repeated:?}"
    );
    assert_eq!(repeated.body.get("appended"), Some(&json!(false)));
    assert_eq!(repeated.body["operation_id"], accepted["operation_id"]);
    let copies:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM native_sip_intakes WHERE call_id='call.raw.keep-001'),(SELECT count(*) FROM native_objects WHERE kind='realtime_call' AND raw_id='call.raw.keep-001')")
        .fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(copies, (1, 1));
    let mut wrong_route = delivery;
    wrong_route["route_id"] = json!("caller-chosen-tenant-route");
    assert_eq!(
        post(&f.dp, "live/sip-intake", wrong_route).await.status,
        404
    );
    let stale = sip_delivery(
        &fresh,
        "trusted-route-a",
        &secret,
        "delivery.stale",
        "event.stale",
        "call.stale",
    );
    let mut stale = stale;
    stale["webhook_timestamp"] = json!((Utc::now().timestamp() - 301).to_string());
    let stale_key = base64::engine::general_purpose::STANDARD
        .decode(secret.strip_prefix("whsec_").unwrap())
        .unwrap();
    let stale_payload = format!(
        "{}.{}.{}",
        stale["webhook_id"].as_str().unwrap(),
        stale["webhook_timestamp"].as_str().unwrap(),
        stale["raw_body"].as_str().unwrap()
    );
    stale["webhook_signature"] = json!(format!(
        "v1,{}",
        base64::engine::general_purpose::STANDARD.encode(
            ring::hmac::sign(
                &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &stale_key),
                stale_payload.as_bytes()
            )
            .as_ref()
        )
    ));
    assert_eq!(post(&f.dp, "live/sip-intake", stale).await.status, 404);
}

#[tokio::test]
async fn a_live_usage_is_debited_once_and_close_settles_only_the_priced_received_amount() {
    let Some(f) = fixture().await else { return };
    support::world::price(&f.cp, f.model).await;
    let (accepted, session) = create(&f.dp, &f.template, None).await;
    let body = usage(&accepted, &session, "charged.event.1", "charged.response.1");
    let first = post(&f.dp, "live/append", body.clone()).await;
    assert_eq!(first.status, 200, "{first:?}");
    assert_ne!(
        first.body["live_session"]["accounting_state"], "settled",
        "the open session is not complete accounting coverage"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            let count:i64=sqlx::query_scalar("SELECT count(*) FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id JOIN billing_ledger_cny ledger ON ledger.event_id=outbox.event_id::text WHERE journal.operation_id=$1")
                .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
            if count==1 {break;}
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("live observed usage must reach the existing real billing ledger before close");
    let closed = post(
        &f.dp,
        "live/checkpoint",
        json!({"lease":session["lease"],"terminal":true,"coverage":"complete"}),
    )
    .await;
    assert_eq!(closed.status, 200, "{closed:?}");
    assert_eq!(closed.body["live_session"]["state"], "closed");
    assert_eq!(
        closed.body["live_session"]["accounting_state"], "settled",
        "delivery before close must still become settled"
    );
    for _ in 0..2 {
        let replay = post(&f.dp, "live/append", body.clone()).await;
        assert_eq!(replay.status, 200, "{replay:?}");
        assert_eq!(replay.body.get("appended"), Some(&json!(false)));
    }
    let counts:(i64,i64)=sqlx::query_as("SELECT count(*),COALESCE(sum(-ledger.amount_micro_cny),0)::bigint FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id JOIN billing_ledger_cny ledger ON ledger.event_id=outbox.event_id::text WHERE journal.operation_id=$1")
        .bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.unwrap();
    assert_eq!(
        counts,
        (1, 15),
        "8 uncached input + 2 cached×0.1 + 3 output×2 = 14.2, rounded once to 15 micro-CNY"
    );
}

#[tokio::test]
async fn heartbeat_after_current_key_revocation_requires_stop_without_losing_observed_tail() {
    let Some(f) = fixture().await else { return };
    let (accepted, session) = create(&f.dp, &f.template, None).await;
    sqlx::query("UPDATE tenant_keys SET disabled=true WHERE id=$1")
        .bind(f.key)
        .execute(f.cp.db().pool())
        .await
        .unwrap();
    let heartbeat = post(
        &f.dp,
        "live/checkpoint",
        json!({"lease":session["lease"],"coverage":"partial"}),
    )
    .await;
    assert_eq!(
        heartbeat.status, 200,
        "an active observer receives an actionable stop verdict: {heartbeat:?}"
    );
    assert_eq!(heartbeat.body["live_session"]["requires_stop"], true);
    assert_eq!(heartbeat.body["live_session"]["coverage"], "unknown");
    assert_eq!(
        heartbeat.body["live_session"]["deadline_at"],
        session["deadline_at"]
    );
    let tail = post(
        &f.dp,
        "live/append",
        usage(
            &accepted,
            &session,
            "revoked.tail.event",
            "revoked.tail.response",
        ),
    )
    .await;
    assert_eq!(
        tail.status, 200,
        "revocation cannot erase already-reported paid work: {tail:?}"
    );
    let cleanup = post(
        &f.dp,
        "live/checkpoint",
        json!({"lease":session["lease"],"coverage":"unknown","terminal":true}),
    )
    .await;
    assert_eq!(
        cleanup.status, 200,
        "cleanup remains reachable after revocation: {cleanup:?}"
    );
    assert_eq!(cleanup.body["live_session"]["state"], "closed");
    assert_eq!(
        cleanup.body["live_session"]["accounting_state"],
        "awaiting_reconciliation"
    );
}

#[tokio::test]
async fn a_terminal_partial_report_does_not_become_complete_coverage() {
    let Some(f) = fixture().await else { return };
    let (accepted, session) = create(&f.dp, &f.template, None).await;
    let mut report = usage(
        &accepted,
        &session,
        "partial.tail.event",
        "partial.tail.response",
    );
    report["terminal"] = json!(true);
    report["coverage"] = json!("partial");
    let closed = post(&f.dp, "live/append", report).await;
    assert_eq!(closed.status, 200, "{closed:?}");
    assert_eq!(closed.body["live_session"]["state"], "closed");
    assert_eq!(
        closed.body["live_session"]["coverage"], "partial",
        "transport termination cannot prove complete usage coverage"
    );
    assert_eq!(
        closed.body["live_session"]["accounting_state"],
        "awaiting_reconciliation"
    );
    let quantities: Value =
        sqlx::query_scalar("SELECT usage FROM native_live_journal WHERE operation_id=$1")
            .bind(id(&accepted, "/operation_id"))
            .fetch_one(f.cp.db().pool())
            .await
            .unwrap();
    assert_eq!(
        quantities.pointer("/0/meter/usage/prompt_tokens"),
        Some(&json!(10)),
        "reported quantities remain billable even when the interval is incomplete"
    );
}

// 空终态只收束已接受的观察，不新增用量，也不续租。
async fn terminal_empty_snapshot(f: &Fixture, operation: Uuid) -> Value {
    sqlx::query_scalar(
        r#"SELECT jsonb_build_object(
          'identity',(SELECT to_jsonb(live)-ARRAY['state','coverage','accounting_state','closed_at','updated_at']::text[]
            FROM native_live_sessions live WHERE environment_id=$1 AND operation_id=$2),
          'entries',(SELECT COALESCE(jsonb_agg(to_jsonb(entry) ORDER BY entry_id),'[]'::jsonb)
            FROM native_live_entries entry WHERE environment_id=$1 AND operation_id=$2),
          'journal',(SELECT COALESCE(jsonb_agg(to_jsonb(journal) ORDER BY event_id),'[]'::jsonb)
            FROM native_live_journal journal WHERE environment_id=$1 AND operation_id=$2),
          'outbox',(SELECT COALESCE(jsonb_agg(to_jsonb(outbox)-ARRAY['delivered_at','pricing_state']::text[] ORDER BY outbox.event_id),'[]'::jsonb)
            FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id
            WHERE journal.environment_id=$1 AND journal.operation_id=$2),
          'ledger',(SELECT COALESCE(jsonb_agg(to_jsonb(ledger) ORDER BY ledger.event_id),'[]'::jsonb)
            FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id
            JOIN billing_ledger_cny ledger ON ledger.event_id=outbox.event_id::text
            WHERE journal.environment_id=$1 AND journal.operation_id=$2))"#,
    )
    .bind(f.cp.state().env.env_id)
    .bind(operation)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("read actual identities and immutable accepted usage facts")
}

async fn assert_terminal_empty_close(f: &Fixture, accepted: &Value, session: &Value) {
    let operation = id(accepted, "/operation_id");
    let original_expiry: chrono::DateTime<Utc> =
        serde_json::from_value(session["lease_expires_at"].clone())
            .expect("create explicitly returned the original lease expiry");
    let before = terminal_empty_snapshot(f, operation).await;
    let request =
        json!({"lease":session["lease"],"terminal":true,"coverage":"unknown","entries":[]});
    let closed = post(&f.dp, "live/checkpoint", request.clone()).await;
    assert_eq!(
        closed.status, 200,
        "accepted empty terminal cleanup: {closed:?}"
    );
    let record = &closed.body["live_session"];
    assert_eq!(record["operation_id"], accepted["operation_id"]);
    assert_eq!(record["api_key_id"], accepted["api_key_id"]);
    assert_eq!(record["lease"], session["lease"]);
    assert_eq!(record["state"], "closed");
    assert_eq!(record["coverage"], "unknown");
    assert_eq!(record["accounting_state"], "awaiting_reconciliation");
    let actual_expiry: chrono::DateTime<Utc> =
        serde_json::from_value(record["lease_expires_at"].clone())
            .expect("close explicitly returns its lease expiry");
    assert_eq!(
        actual_expiry, original_expiry,
        "terminal cleanup cannot extend the last accepted lease"
    );
    let persisted: (String, String, String, bool) = sqlx::query_as(
        "SELECT state,coverage,accounting_state,closed_at IS NOT NULL FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(f.cp.state().env.env_id)
    .bind(operation)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("read the durably closed lifecycle");
    assert_eq!(
        persisted,
        (
            "closed".into(),
            "unknown".into(),
            "awaiting_reconciliation".into(),
            true
        )
    );
    assert_eq!(terminal_empty_snapshot(f, operation).await, before,
        "empty close preserves holder, generation, deadline, expiry, entries, nonzero quantities, outbox and existing debits");
    let closed_at: Value = sqlx::query_scalar(
        "SELECT to_jsonb(closed_at) FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(f.cp.state().env.env_id)
    .bind(operation)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("read explicit durable closed_at");
    assert!(!closed_at.is_null());
    let replay = post(&f.dp, "live/checkpoint", request).await;
    assert_eq!(
        replay.status, 200,
        "same-holder terminal replay: {replay:?}"
    );
    assert_eq!(replay.body["live_session"], closed.body["live_session"]);
    let replay_closed_at: Value = sqlx::query_scalar(
        "SELECT to_jsonb(closed_at) FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(f.cp.state().env.env_id)
    .bind(operation)
    .fetch_one(f.cp.db().pool())
    .await
    .expect("read replayed closed_at");
    assert_eq!(replay_closed_at, closed_at);
    assert_eq!(terminal_empty_snapshot(f, operation).await, before);
}

#[tokio::test]
async fn terminal_empty_checkpoint_after_natural_expiry_closes_unknown_without_new_usage() {
    let Some(f) = fixture().await else { return };
    support::world::price(&f.cp, f.model).await;
    let accepted = begin(&f.dp, &f.template).await;
    let created = post(
        &f.dp,
        "live/create",
        json!({
            "operation_id":accepted["operation_id"],"api_key_id":accepted["api_key_id"],
            "purpose":"realtime","transport":"web_socket",
            "provider_session_id":format!("sess_terminal_expiry.{}",Uuid::new_v4()),
            "deadline_at":Utc::now()+Duration::seconds(4)
        }),
    )
    .await;
    assert_eq!(
        created.status, 200,
        "public short-deadline admission: {created:?}"
    );
    let session = created.body["live_session"].clone();
    assert_eq!(session["state"], "live");
    assert!(session
        .pointer("/lease/generation")
        .and_then(Value::as_i64)
        .is_some_and(|generation| generation > 0));
    let report = usage(
        &accepted,
        &session,
        "terminal.expiry.nonzero",
        "terminal.expiry.response",
    );
    let appended = post(&f.dp, "live/append", report).await;
    assert_eq!(
        appended.status, 200,
        "nonzero accepted fact before close: {appended:?}"
    );
    assert_eq!(appended.body.get("appended"), Some(&json!(true)));
    assert_eq!(appended.body["live_session"]["state"], "live");
    assert_eq!(appended.body["live_session"]["coverage"], "complete");
    let operation = id(&accepted, "/operation_id");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let count:i64=sqlx::query_scalar("SELECT count(*) FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id JOIN billing_ledger_cny ledger ON ledger.event_id=outbox.event_id::text WHERE journal.operation_id=$1")
                .bind(operation).fetch_one(f.cp.db().pool()).await.expect("read actual paid positive control");
            if count == 1 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("the real nonzero outbox reaches the existing billing ledger before cleanup");
    let facts = terminal_empty_snapshot(&f, operation).await;
    assert_eq!(facts["journal"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        facts.pointer("/journal/0/usage/0/meter/usage/prompt_tokens"),
        Some(&json!(10))
    );
    assert_eq!(facts["outbox"].as_array().map(Vec::len), Some(1));
    assert_eq!(facts["ledger"].as_array().map(Vec::len), Some(1));
    let (expires, db_now): (chrono::DateTime<Utc>, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT lease_expires_at,clock_timestamp() FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2",
    ).bind(f.cp.state().env.env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("read the real unchanged lease and database clock");
    let returned_expiry: chrono::DateTime<Utc> =
        serde_json::from_value(session["lease_expires_at"].clone())
            .expect("explicit admitted expiry");
    assert_eq!(expires, returned_expiry);
    let wait = expires
        .signed_duration_since(db_now)
        .to_std()
        .unwrap_or_default();
    tokio::time::sleep(wait + std::time::Duration::from_millis(25)).await;
    let expired: bool = sqlx::query_scalar(
        "SELECT lease_expires_at<=clock_timestamp() AND deadline_at<=clock_timestamp() FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2",
    ).bind(f.cp.state().env.env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("observe natural deadline/lease expiry without clock mutation");
    assert!(
        expired,
        "the public admission naturally reaches its own persisted deadline and lease expiry"
    );
    assert_terminal_empty_close(&f, &accepted, &session).await;
}

#[tokio::test]
async fn terminal_empty_checkpoint_before_expiry_preserves_original_expiry() {
    let Some(f) = fixture().await else { return };
    let (accepted, session) = create(&f.dp, &f.template, None).await;
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let live: bool = sqlx::query_scalar(
        "SELECT lease_expires_at>clock_timestamp() AND state='live' FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2",
    ).bind(f.cp.state().env.env_id).bind(id(&accepted,"/operation_id")).fetch_one(f.cp.db().pool()).await.expect("the delayed terminal request still has a genuinely live lease");
    assert!(live);
    let facts = terminal_empty_snapshot(&f, id(&accepted, "/operation_id")).await;
    assert_eq!(facts["journal"].as_array().map(Vec::len), Some(0));
    assert_eq!(facts["outbox"].as_array().map(Vec::len), Some(0));
    assert_terminal_empty_close(&f, &accepted, &session).await;
}
