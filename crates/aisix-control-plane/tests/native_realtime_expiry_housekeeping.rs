//! 真实 CP/PG/mTLS 的过期 WS authority 生命周期与实例重启组件合同。
//! 没有供应商 WS、真实 GW 进程崩溃或 provider 重拨观测；不以恒零计数冒充这些证明。
//! 启动沿生产 housekeeping spawn/tick；周期幂等检查只调用同一公开 tick，不替换成 run_once。

mod support;

use std::time::Duration as WallDuration;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::housekeeping;
use aisix_control_plane::resources::write::WriteCtx;
use chrono::{DateTime, Duration, Utc};
use ring::hmac;
use serde_json::{json, Value};
use support::{Bundle, DpClient, OperatorSession, TestCp};
use uuid::Uuid;

const PREFIX: &str = "/dp/v1/native-objects";
const BASE: &str = "https://realtime-authority.example/v1";
const PROVIDER_KEY: &str = "sk-realtime-fixture-not-a-provider-secret";
const PROJECT: &str = "proj_realtime_authority_fixture";
const MODEL: &str = "expiry-authority-main";
const ASR: &str = "expiry-authority-asr";
const ASR_UPSTREAM: &str = "gpt-4o-mini-transcribe";
const ENTRY: &str = "asr.accepted.before.expiry";
const LIMIT: WallDuration = WallDuration::from_secs(15);

fn id(value: &Value, pointer: &str) -> Uuid {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|text| text.parse().ok())
        .expect("公开响应明确给出非空 resource UUID")
}

// 逐字复用既有 authority fixture 的静态账号边界 setup，不新增观察 realm 或身份协议。
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
    dp: DpClient,
    bundle: Bundle,
    tenant: Uuid,
    key: Uuid,
    model: Uuid,
    asr: Uuid,
    template: Value,
}

async fn admin_post(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let response = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(response.status, 201, "真实 public Admin fixture 写入成功");
    response.body
}

async fn fixture() -> Option<Fixture> {
    let cp = TestCp::start().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let provider = admin_post(&cp, &admin, "/admin/resources/provider_keys", json!({"doc":{
        "display_name":"expiry-authority-account","provider":"openai","api_key":PROVIDER_KEY,
        "api_base":BASE,"groups":["shared"],"request":{"default_headers":{"OpenAI-Project":PROJECT}}
    }})).await;
    let provider = id(&provider, "/id");
    let mut models = Vec::new();
    for (alias, upstream) in [(MODEL, "gpt-realtime"), (ASR, ASR_UPSTREAM)] {
        let model = admin_post(&cp, &admin, "/admin/resources/models", json!({"doc":{
            "display_name":alias,"provider":"openai","model_name":upstream,"provider_key_id":provider
        }})).await;
        models.push(id(&model, "/id"));
    }
    let (model, asr) = (models[0], models[1]);
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    let path = format!("/admin/tenants/{tenant}");
    let current = cp.send("GET", &path, Some(admin.cookie()), None).await;
    assert_eq!(current.status, 200);
    let granted = cp
        .send_conditional(
            "PATCH",
            &path,
            admin.cookie(),
            current.body["version"]
                .as_i64()
                .expect("实际 tenant version"),
            json!({"allowed_model_ids":[model,asr]}),
        )
        .await;
    assert_eq!(granted.status, 200);
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let group = support::world::group_id(&cp, "shared").await;
    let key = admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label":"expiry-authority-key","member_id":member,"group_id":group}),
    )
    .await;
    let key = id(&key, "/key/id");
    admin_post(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/credits"),
        json!({"amount_cny":"1","reason":"自然过期 authority 生命周期组件正控"}),
    )
    .await;
    admin_post(&cp, &admin, &format!("/admin/provider-groups/{group}/rates"),
        json!({"model_id":null,"cny_per_usd":"1","effective_from":support::world::db_now(&cp).await})).await;
    for (model, card) in [
        (model, json!({"input":"10","cache_read":"2","output":"20"})),
        (
            asr,
            json!({"input":"100","audio_input":"100","output":"200","audio_output":"200","cache_read":"20"}),
        ),
    ] {
        admin_post(
            &cp,
            &admin,
            "/admin/pricing/baselines",
            json!({
                "model_id":model,"effective_from":support::world::db_now(&cp).await,
                "note":"既有公开 token/audio 维度的非零价格正控","card":{"default":{"base":card}}
            }),
        )
        .await;
    }
    let bundle = cp
        .issue_bundle(&admin, "expiry-authority-original-producer")
        .await;
    let dp = cp.dp_client(&bundle);
    let template = json!({
        "operation_id":Uuid::new_v4(),"api_key_id":key,"protocol":"openai","kind":"realtime_call",
        "origin":{"model_id":model,"provider_key_id":provider,"boundary":boundary(&cp,provider)},
        "requested_model_id":model,"requested_model":MODEL,"upstream_model":"gpt-realtime",
        "upstream_provider":"openai","operation":"realtime","request_id":"expiry-authority-contract",
        "asynchronous":false
    });
    Some(Fixture {
        cp,
        dp,
        bundle,
        tenant,
        key,
        model,
        asr,
        template,
    })
}

async fn post(dp: &DpClient, operation: &str, body: Value) -> Value {
    let response = dp
        .send("POST", &format!("{PREFIX}/{operation}"), Some(body))
        .await;
    assert_eq!(
        response.status, 200,
        "真实 authenticated authority 请求成功；不打印私有响应体"
    );
    response.body
}

async fn create(f: &Fixture, seconds: i64, label: &str) -> (Value, Value) {
    let mut accepted = f.template.clone();
    accepted["operation_id"] = json!(Uuid::new_v4());
    accepted["request_id"] = json!(format!("expiry-authority-{label}"));
    let begun = post(&f.dp, "begin", accepted.clone()).await;
    assert!(begun.get("operation_id") == accepted.get("operation_id"));
    let created = post(&f.dp, "live/create", json!({
        "operation_id":accepted["operation_id"],"api_key_id":f.key,"purpose":"realtime","transport":"web_socket",
        "provider_session_id":format!("sess_authority_{label}.{}",Uuid::new_v4()),
        "deadline_at":Utc::now()+Duration::seconds(seconds)
    })).await;
    let session = created
        .get("live_session")
        .expect("实际 public live_session")
        .clone();
    assert!(
        session.get("operation_id") == accepted.get("operation_id")
            && session.get("api_key_id") == Some(&json!(f.key))
    );
    assert_eq!(session.get("state"), Some(&json!("live")));
    assert!(session
        .pointer("/lease/generation")
        .and_then(Value::as_i64)
        .is_some_and(|value| value > 0));
    let expires: DateTime<Utc> = serde_json::from_value(session["lease_expires_at"].clone())
        .expect("明确返回实际 lease expiry");
    let deadline: DateTime<Utc> = serde_json::from_value(session["deadline_at"].clone())
        .expect("明确返回实际 lifecycle deadline");
    let now = support::world::db_now(&f.cp).await;
    assert!(
        expires > now && deadline > now,
        "公开准入确实给出真实未来 lease/deadline，不以已过期 fixture 假绿"
    );
    (accepted, session)
}

fn usage(accepted: &Value, session: &Value, event: &str, response: &str) -> Value {
    // 复用公开 authority usage 形状；这里是明确的协议观察量，不冒充真实供应商处理。
    json!({"operation_id":accepted["operation_id"],"api_key_id":accepted["api_key_id"],
        "event_id":event,"response_id":response,"lease":session["lease"],"coverage":"complete","terminal":false,
        "usage":[{"entry_id":"","upstream_model":"gpt-realtime","occurred_at":Utc::now(),
            "meter":{"kind":"tokens","usage":{"entry_id":"","occurred_at":Utc::now(),"status_code":200,
                "prompt_tokens":10,"completion_tokens":3,"cached_prompt_tokens":2,"total_tokens":13}}}]})
}

async fn lifecycle(f: &Fixture, operation: Uuid) -> Value {
    let row: Value = sqlx::query_scalar("SELECT to_jsonb(live) FROM native_live_sessions live WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("只读实际持久 lifecycle");
    assert!(row.is_object(), "原 operation 的 lifecycle 必须实际存在");
    row
}

fn identity(row: &Value) -> Value {
    let mut row = row.as_object().expect("真实 lifecycle object").clone();
    for field in [
        "state",
        "coverage",
        "accounting_state",
        "closed_at",
        "updated_at",
    ] {
        row.remove(field);
    }
    Value::Object(row)
}

async fn material(f: &Fixture) -> Value {
    // 排除异步 delivery 标记；先等真实定价和交付，再严格比较已接受身份、数量、币种与金额。
    sqlx::query_scalar(r#"SELECT jsonb_build_object(
      'operations',(SELECT COALESCE(jsonb_agg(to_jsonb(op)-'state' ORDER BY operation_id),'[]'::jsonb)
        FROM native_operations op WHERE environment_id=$1),
      'entries',(SELECT COALESCE(jsonb_agg(to_jsonb(accepted_entry) ORDER BY operation_id,entry_id),'[]'::jsonb)
        FROM native_live_entries accepted_entry WHERE environment_id=$1),
      'journal',(SELECT COALESCE(jsonb_agg(to_jsonb(journal) ORDER BY id),'[]'::jsonb)
        FROM native_live_journal journal WHERE environment_id=$1),
      'outbox',(SELECT COALESCE(jsonb_agg(to_jsonb(outbox)-ARRAY['delivered_at','pricing_state']::text[] ORDER BY outbox.event_id),'[]'::jsonb)
        FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.environment_id=$1),
      'events',(SELECT COALESCE(jsonb_agg(to_jsonb(usage_row) ORDER BY event_id),'[]'::jsonb)
        FROM usage_events usage_row WHERE api_key_id=$2),
      'charges',(SELECT COALESCE(jsonb_agg(to_jsonb(charge) ORDER BY event_id),'[]'::jsonb)
        FROM pricing_charges_cny charge WHERE tenant_id=$3::text),
      'ledger',(SELECT COALESCE(jsonb_agg(to_jsonb(ledger) ORDER BY event_id),'[]'::jsonb)
        FROM billing_ledger_cny ledger WHERE tenant_id=$3 AND kind='usage'),
      'balance',(SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$3))"#)
        .bind(f.cp.env().env_id).bind(f.key.to_string()).bind(f.tenant)
        .fetch_one(f.cp.db().pool()).await.expect("只读原 accepted entries、来源、账本和余额；不写 marker")
}

async fn paid(f: &Fixture, a: i64, b: i64) {
    tokio::time::timeout(LIMIT,async {
        loop {
            let rows: Vec<(String,i64)> = sqlx::query_as("SELECT requested_model_id,charge_micro_cny FROM pricing_charges_cny WHERE tenant_id=$1 AND status='charged'")
                .bind(f.tenant.to_string()).fetch_all(f.cp.db().pool()).await.expect("实际非零收费行");
            let sum_a: i64 = rows.iter().filter(|row|row.0==f.model.to_string()).map(|row|row.1).sum();
            let sum_b: i64 = rows.iter().filter(|row|row.0==f.asr.to_string()).map(|row|row.1).sum();
            assert!(rows.len()<=((a>0) as usize+(b>0) as usize) && sum_a<=a && sum_b<=b,"不能靠重复或第三模型收费达到正控金额");
            let delivered: i64 = sqlx::query_scalar("SELECT count(*) FROM native_live_outbox outbox JOIN native_live_journal journal ON journal.id=outbox.journal_id WHERE journal.environment_id=$1 AND outbox.delivered_at IS NOT NULL AND outbox.pricing_state='charged'")
                .bind(f.cp.env().env_id).fetch_one(f.cp.db().pool()).await.expect("实际异步 outbox 交付确认");
            let expected = (a>0) as i64+(b>0) as i64;
            if sum_a==a && sum_b==b && delivered==expected {
                let money: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'),(SELECT COALESCE(sum(-amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'),(SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1)")
                    .bind(f.tenant).fetch_one(f.cp.db().pool()).await.expect("真实 ledger 与 CNY balance");
                assert_eq!(money,(expected,a+b,1_000_000-a-b));
                break;
            }
            tokio::time::sleep(WallDuration::from_millis(20)).await;
        }
    }).await.expect("真实 received quantities 经 outbox 和 pricing 进入实际 ledger，不能缺价或 paid0");
}

async fn wait_expired(f: &Fixture, operation: Uuid, expected: &Value) {
    let returned_expiry: DateTime<Utc> =
        serde_json::from_value(expected["lease_expires_at"].clone())
            .expect("最后一次生产请求明确返回 expiry");
    let returned_deadline: DateTime<Utc> = serde_json::from_value(expected["deadline_at"].clone())
        .expect("最后一次生产请求明确返回 deadline");
    tokio::time::timeout(WallDuration::from_secs(60),async {
        loop {
            let (expiry,deadline,now): (DateTime<Utc>,DateTime<Utc>,DateTime<Utc>) = sqlx::query_as("SELECT lease_expires_at,deadline_at,clock_timestamp() FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2")
                .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("实际数据库时间与未篡改的 expiry/deadline");
            assert!(expiry==returned_expiry && deadline==returned_deadline,"producer 停止后没有续租或手改到期时间");
            if expiry<=now && deadline<=now { break; }
            tokio::time::sleep(WallDuration::from_millis(25)).await;
        }
    }).await.expect("短 public admission 在真实时间自然到期；不改 clock/数据库 expiry");
}

async fn restart_owned(f: Fixture) -> Fixture {
    let Fixture {
        cp: old,
        dp,
        bundle,
        tenant,
        key,
        model,
        asr,
        template,
    } = f;
    let env = old.env().env_id;
    let platform_key = old.state().pki.platform_id_key_hex();
    drop(dp);
    // restart(&self) 有短暂实例重叠；获得新 listener 后先释放旧 owned CP，再进行任何恢复观测。
    let cp = old.restart().await;
    drop(old);
    assert!(
        cp.env().env_id == env && cp.state().pki.platform_id_key_hex() == platform_key,
        "同 DB/environment/platform key 的真实 authority 实例重启；不输出任何 key"
    );
    let dp = cp.dp_client(&bundle);
    Fixture {
        cp,
        dp,
        bundle,
        tenant,
        key,
        model,
        asr,
        template,
    }
}

async fn live_control(f: &Fixture, operation: Uuid) {
    let active: bool = sqlx::query_scalar("SELECT state='live' AND lease_expires_at>clock_timestamp() AND deadline_at>clock_timestamp() FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("明确仍未到期的真实 WS control");
    assert!(
        active,
        "unexpired control 必须在观察时仍真实未到期，不能只看长 deadline"
    );
}

#[tokio::test]
async fn production_boot_housekeeping_closes_naturally_expired_ws_unknown_and_preserves_paid_facts_across_restart(
) {
    let Some(f) = fixture().await else { return };
    // complete control 先有真实 A144，再正常收束；不使用空计量假造已完成业务。
    let (closed_accepted, closed_session) = create(&f, 600, "closed_control").await;
    let closed_operation = id(&closed_accepted, "/operation_id");
    let report = usage(
        &closed_accepted,
        &closed_session,
        "closed.control.known.event",
        "closed.control.known.response",
    );
    let appended = post(&f.dp, "live/append", report).await;
    assert_eq!(appended.get("appended"), Some(&json!(true)));
    paid(&f, 144, 0).await;
    let completed = post(
        &f.dp,
        "live/checkpoint",
        json!({"lease":closed_session["lease"],"terminal":true,"coverage":"complete","entries":[]}),
    )
    .await;
    assert!(
        completed.pointer("/live_session/state") == Some(&json!("closed"))
            && completed.pointer("/live_session/coverage") == Some(&json!("complete"))
            && completed.pointer("/live_session/accounting_state") == Some(&json!("settled")),
        "实际已计费的 complete control 在重启前已经 closed/settled"
    );
    let (accepted, session) = create(&f, 15, "natural_expiry_target").await;
    let operation = id(&accepted, "/operation_id");
    let entry = json!({"entry_id":ENTRY,"endpoint":"/v1/audio/transcriptions","requested_model_id":f.asr,
        "requested_model":ASR,"model_id":f.asr,"upstream_model":ASR_UPSTREAM,"upstream_provider":"openai"});
    let registered = post(
        &f.dp,
        "live/checkpoint",
        json!({"lease":session["lease"],"coverage":"partial","entries":[entry.clone()]}),
    )
    .await;
    assert!(
        registered.get("live_session").is_some(),
        "真实 checkpoint 确认已授权 B entry"
    );
    let stored: Vec<Value> = sqlx::query_scalar(
        "SELECT entry FROM native_live_entries WHERE environment_id=$1 AND operation_id=$2",
    )
    .bind(f.cp.env().env_id)
    .bind(operation)
    .fetch_all(f.cp.db().pool())
    .await
    .expect("实际 accepted B entry 正控");
    assert!(
        stored.len() == 1 && stored[0] == entry,
        "entry 非空且精确属于实际获准 B，不拿空集合证明保全"
    );
    let last_session = registered["live_session"].clone();
    let active: bool = sqlx::query_scalar("SELECT lease_expires_at>clock_timestamp() AND deadline_at>clock_timestamp() FROM native_live_sessions WHERE environment_id=$1 AND operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("非零 append 前实际准入未到期");
    assert!(active);
    let mut b_report = usage(
        &accepted,
        &last_session,
        "expiry.target.nonzero.b",
        "expiry.target.accepted.item:0",
    );
    b_report["coverage"] = json!("partial");
    b_report["usage"][0]["entry_id"] = json!(ENTRY);
    b_report["usage"][0]["upstream_model"] = json!(ASR_UPSTREAM);
    b_report["usage"][0]["meter"]["usage"] = json!({"entry_id":ENTRY,"occurred_at":Utc::now(),"status_code":200,
        "prompt_tokens":5,"prompt_audio_tokens":5,"completion_tokens":1,"total_tokens":6});
    b_report["raw_usage"] = json!({"type":"tokens","input_tokens":5,"output_tokens":1,"total_tokens":6,"input_token_details":{"audio_tokens":5,"text_tokens":0}});
    let observed = post(&f.dp, "live/append", b_report).await;
    assert_eq!(observed.get("appended"), Some(&json!(true)));
    let stopped_session = observed["live_session"].clone();
    assert_eq!(
        stopped_session.get("state"),
        Some(&json!("live")),
        "目标 producer 没有发送 terminal"
    );
    paid(&f, 144, 700).await;
    let pre_expiry_fact: bool = sqlx::query_scalar("SELECT journal.created_at<live.lease_expires_at AND journal.created_at<live.deadline_at FROM native_live_journal journal JOIN native_live_sessions live USING(environment_id,operation_id) WHERE live.environment_id=$1 AND live.operation_id=$2")
        .bind(f.cp.env().env_id).bind(operation).fetch_one(f.cp.db().pool()).await.expect("非零 known fact 确实在原实际到期前持久接受");
    assert!(pre_expiry_fact);
    // 从这里起不再向目标 append/checkpoint/renew/terminal；自然时钟和生产 boot 负责后续。
    wait_expired(&f, operation, &stopped_session).await;
    let (unexpired_accepted, _) = create(&f, 600, "unexpired_control").await;
    let unexpired_operation = id(&unexpired_accepted, "/operation_id");
    live_control(&f, unexpired_operation).await;
    let before = lifecycle(&f, operation).await;
    assert_eq!(
        before.get("state"),
        Some(&json!("live")),
        "旧 boot 已早于该 session；自然到期目标尚未 terminal"
    );
    assert!(before.get("closed_at").is_some_and(Value::is_null));
    assert!(before
        .get("lease_dp_id")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<Uuid>().ok())
        .is_some_and(|holder| !holder.is_nil()));
    assert!(before
        .get("lease_generation")
        .and_then(Value::as_i64)
        .is_some_and(|generation| generation > 0));
    let closed_before = lifecycle(&f, closed_operation).await;
    let unexpired_before = lifecycle(&f, unexpired_operation).await;
    let known_before = material(&f).await;
    for (field, count) in [
        ("operations", 3),
        ("entries", 1),
        ("journal", 2),
        ("outbox", 2),
        ("events", 2),
        ("charges", 2),
        ("ledger", 2),
    ] {
        assert_eq!(
            known_before
                .get(field)
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(count),
            "每类被保全资料有真实非空正控: {field}"
        );
    }
    assert_eq!(known_before.get("balance"), Some(&json!(999156)));
    let f = restart_owned(f).await;
    let after = tokio::time::timeout(LIMIT,async {
        loop {
            let row = lifecycle(&f,operation).await;
            if row.get("state")==Some(&json!("closed")) { break row; }
            tokio::time::sleep(WallDuration::from_millis(20)).await;
        }
    }).await.expect("实际 production boot tick 有界收束自然到期 WS；不能只重建 AppState 或凭 sleep 声称维护已跑");
    assert!(
        after.get("coverage") == Some(&json!("unknown"))
            && after.get("accounting_state") == Some(&json!("awaiting_reconciliation"))
            && after.get("closed_at").is_some_and(|value| !value.is_null()),
        "缺 terminal 的自然到期生命周期只收束 Unknown，不能声称完整或 paid0"
    );
    assert!(
        identity(&after) == identity(&before),
        "holder/generation/expiry/deadline/原归属与固定来源不能被 housekeeping 改写"
    );
    live_control(&f, unexpired_operation).await;
    assert!(
        lifecycle(&f, closed_operation).await == closed_before,
        "既有 closed/complete control 不被重置为 Unknown"
    );
    assert!(
        lifecycle(&f, unexpired_operation).await == unexpired_before,
        "真实未到期 control 保持原 lease/lifecycle"
    );
    assert!(material(&f).await==known_before,"原非空 entries/opaque operation/source/known usage/outbox/币种金额必须全保，不制造 usage0 或额外 authority operation");
    paid(&f, 144, 700).await;
    for _ in 0..2 {
        housekeeping::tick(f.cp.db(), &WriteCtx::of(f.cp.state())).await;
        assert!(
            lifecycle(&f, operation).await == after,
            "重复 production tick 必须保留 closed_at/updated_at 与全部生命周期字段"
        );
        live_control(&f, unexpired_operation).await;
        assert!(lifecycle(&f, unexpired_operation).await == unexpired_before);
        assert!(lifecycle(&f, closed_operation).await == closed_before);
        assert!(
            material(&f).await == known_before,
            "重复维护不新增账务/accepted entry 或改写来源"
        );
        paid(&f, 144, 700).await;
    }
}
