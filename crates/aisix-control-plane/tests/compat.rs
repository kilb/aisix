//! The save-time gateway-version gate: a document carrying a field that
//! gateways older than its minimum version would misread is refused while
//! such a gateway has reported in the last 24 hours, and while the control
//! plane cannot tell which versions its gateways run.
//!
//! Gateways are registered the way real ones are: a credential is issued and
//! its gateway sends heartbeats (`support::DpClient`).

mod support;

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::error::ApiError;
use aisix_control_plane::resources::write::{write_resource, ResourceWrite, WriteCtx, WriteOp};
use aisix_control_plane::resources::Kind;
use futures::FutureExt;
use serde_json::{json, Value};
use support::{DpClient, OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// The release this control plane reports, as a stamped build would.
const CP_VERSION: &str = "1.9.0+sha-cp7c1";

/// A gateway older than [`CP_VERSION`].
const OLD: &str = "1.8.2+sha-old7c1";

/// A gateway at [`CP_VERSION`]'s release.
const CURRENT: &str = "1.9.0+sha-new7c1";

/// A registered gateway credential, and the client its gateways use.
struct Gateway {
    dp_id: String,
    client: DpClient,
}

impl Gateway {
    async fn issue(cp: &TestCp, admin: &OperatorSession) -> Gateway {
        let bundle = cp.issue_bundle(admin, "gw-compat-7c1").await;
        Gateway {
            client: cp.dp_client(&bundle),
            dp_id: bundle.dp_id,
        }
    }

    /// A heartbeat of `instance` reporting `version`, which must be taken.
    async fn beat(&self, instance: &str, version: &str) {
        self.beat_with(instance, version, json!([])).await;
    }

    /// [`Gateway::beat`], reporting `partial` as the gateway's
    /// `partially_compatible_resources`.
    async fn beat_with(&self, instance: &str, version: &str, partial: Value) {
        let r = self
            .client
            .heartbeat(json!({
                "dp_id": self.dp_id,
                "instance_id": instance,
                "hostname": format!("host-{instance}"),
                "uptime_seconds": 1,
                "version": version,
                "supported_guardrail_kinds": [],
                "applied_revision": 1,
                "exporter_health": [],
                "partially_compatible_resources": partial,
            }))
            .await;
        assert_eq!(r.status, 200, "beat {instance} at {version}: {r:?}");
    }
}

/// A control plane reporting [`CP_VERSION`], an admin on it, and a
/// registered gateway credential with no instance yet.
async fn setup() -> Option<(TestCp, OperatorSession, Gateway)> {
    let cp = TestCp::start_stamped(CP_VERSION).await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = Gateway::issue(&cp, &admin).await;
    Some((cp, admin, gw))
}

fn team_policy(name: &str) -> Value {
    json!({"name": name, "scope": "team"})
}

async fn create(cp: &TestCp, admin: &OperatorSession, kind: &str, doc: Value) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/resources/{kind}"),
        Some(admin.cookie()),
        Some(json!({ "doc": doc })),
    )
    .await
}

async fn replace(cp: &TestCp, admin: &OperatorSession, created: &Resp, doc: Value) -> Resp {
    let id = created.body["id"].as_str().expect("an id");
    let version = created.body["version"].as_i64().expect("a version");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let response = client
        .put(cp.url(&format!("/admin/resources/cache_policies/{id}")))
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({ "doc": doc }))
        .send()
        .await
        .expect("send the replace");
    let status = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    Resp {
        status,
        body,
        set_cookie: None,
        headers: Default::default(),
    }
}

/// Every cache policy row, tombstones included.
async fn cache_policy_rows(cp: &TestCp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE kind = 'cache_policies'")
        .fetch_one(cp.db().pool())
        .await
        .expect("count cache policies")
}

/// Asserts `r` is the gate's refusal at `path`, and returns its message.
fn refused_at(r: &Resp, path: &str) -> String {
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["code"], "gateway_too_old", "{r:?}");
    assert_eq!(r.body["details"][0]["path"], path, "{r:?}");
    r.body["details"][0]["message"]
        .as_str()
        .expect("a message")
        .to_string()
}

async fn age_instance(cp: &TestCp, instance: &str, interval: &str) {
    sqlx::query(&format!(
        "UPDATE dp_instances SET last_seen_at = now() - interval '{interval}' \
         WHERE instance_id = $1"
    ))
    .bind(instance)
    .execute(cp.db().pool())
    .await
    .expect("age the instance");
}

#[tokio::test]
async fn a_team_scoped_cache_policy_is_refused_while_an_older_gateway_is_registered() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    gw.beat("gw-current-7c1", CURRENT).await;
    gw.beat("gw-old-7c1", OLD).await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c1")).await;
    let message = refused_at(&r, "/scope");
    assert_eq!(message, "需要网关版本 ≥ 1.9.0，当前有 1 个实例低于此版本");
    assert_eq!(cache_policy_rows(&cp).await, 0, "nothing is stored");
    // The gate is about the floored value, not the kind: another scope saves.
    let r = create(
        &cp,
        &admin,
        "cache_policies",
        json!({"name": "key-7c1", "scope": "api_key"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
}

#[tokio::test]
async fn the_same_write_succeeds_once_every_recent_gateway_is_new_enough() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    gw.beat("gw-upgraded-7c1", OLD).await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c2")).await;
    refused_at(&r, "/scope");
    // A pre-release of the minimum is older than the minimum.
    gw.beat("gw-upgraded-7c1", "1.9.0-rc.2+sha-rc7c1").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c2")).await;
    refused_at(&r, "/scope");
    gw.beat("gw-upgraded-7c1", CURRENT).await;
    gw.beat("gw-later-7c1", "1.10.0+sha-later7c1").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c2")).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["doc"]["scope"], "team");
}

#[tokio::test]
async fn an_instance_unseen_for_a_day_does_not_hold_the_gate() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    gw.beat("gw-current-7c3", CURRENT).await;
    gw.beat("gw-gone-7c3", OLD).await;
    // Offline for most of a day: still one of the gateways that may load it.
    age_instance(&cp, "gw-gone-7c3", "23 hours").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c3")).await;
    refused_at(&r, "/scope");
    age_instance(&cp, "gw-gone-7c3", "25 hours").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c3")).await;
    assert_eq!(r.status, 201, "{r:?}");
}

#[tokio::test]
async fn a_gateway_ignoring_a_floored_field_is_listed_as_unsafe() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    gw.beat_with(
        "gw-partial-7c4",
        CURRENT,
        json!([
            {"kind": "api_keys", "field": "allowed_endpoints", "count": 3},
            {"kind": "models", "field": "future_knob_7c4", "count": 1},
        ]),
    )
    .await;
    gw.beat("gw-clean-7c4", CURRENT).await;
    let r = cp.get_with_cookie("/admin/rollout", admin.cookie()).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["unsafe"],
        json!([{
            "instance_id": "gw-partial-7c4",
            "hostname": "host-gw-partial-7c4",
            "kind": "api_keys",
            "field": "allowed_endpoints",
        }]),
        "{r:?}"
    );
    assert_eq!(
        r.body["partially_compatible"],
        json!([
            {"instance_id": "gw-partial-7c4", "kind": "api_keys", "field": "allowed_endpoints", "count": 3},
            {"instance_id": "gw-partial-7c4", "kind": "models", "field": "future_knob_7c4", "count": 1},
        ]),
        "{r:?}"
    );
}

#[tokio::test]
async fn a_gated_write_before_any_gateway_reports_is_written_unchecked() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    // No gateway has registered: there is none to judge.
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c5a")).await;
    assert_eq!(r.status, 201, "{r:?}");
    // One that registered and went quiet is judged by the version it reported.
    gw.beat("gw-quiet-7c5", OLD).await;
    age_instance(&cp, "gw-quiet-7c5", "10 minutes").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c5b")).await;
    refused_at(&r, "/scope");
    gw.beat("gw-quiet-7c5", CURRENT).await;
    age_instance(&cp, "gw-quiet-7c5", "10 minutes").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c5b")).await;
    assert_eq!(r.status, 201, "{r:?}");
}

#[tokio::test]
async fn a_gated_write_is_refused_while_a_credential_recently_hit_its_instance_cap() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    gw.beat("gw-current-7c6", CURRENT).await;
    let hit = |interval: &'static str| {
        let cp = &cp;
        let dp_id = gw.dp_id.clone();
        async move {
            sqlx::query(&format!(
                "UPDATE dp_credentials SET instance_cap_hit_at = now() - interval '{interval}' \
                 WHERE dp_id = $1::uuid"
            ))
            .bind(dp_id)
            .execute(cp.db().pool())
            .await
            .expect("record a cap hit");
        }
    };
    // Instances were turned away: the ones not seen may be older.
    hit("1 hour").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c6")).await;
    let message = refused_at(&r, "/scope");
    assert!(message.contains("实例上限"), "{message}");
    hit("25 hours").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c6")).await;
    assert_eq!(r.status, 201, "{r:?}");
}

#[tokio::test]
async fn a_gateway_whose_version_cannot_be_read_holds_the_gate() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    gw.beat("gw-current-7c7", CURRENT).await;
    for version in ["", "garbled-7c7"] {
        gw.beat("gw-unknown-7c7", version).await;
        let r = create(&cp, &admin, "cache_policies", team_policy("team-7c7")).await;
        let message = refused_at(&r, "/scope");
        assert!(
            message.contains("1 个实例的版本无法识别"),
            "{version:?}: {message}"
        );
    }
    // An unstamped build counts as current.
    gw.beat("gw-unknown-7c7", "dev+sha-dev7c7").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c7")).await;
    assert_eq!(r.status, 201, "{r:?}");
}

#[tokio::test]
async fn an_unstamped_control_plane_cannot_judge_this_release_and_lets_the_write_pass() {
    let Some(cp) = TestCp::start_stamped("dev+sha-cp7c8").await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = Gateway::issue(&cp, &admin).await;
    gw.beat("gw-old-7c8", OLD).await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c8")).await;
    assert_eq!(r.status, 201, "{r:?}");
}

#[tokio::test]
async fn a_replace_that_keeps_a_floored_field_is_not_refused_but_one_that_adds_it_is() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    gw.beat("gw-current-7c9", CURRENT).await;
    let team = create(&cp, &admin, "cache_policies", team_policy("team-7c9")).await;
    assert_eq!(team.status, 201, "{team:?}");
    let other = create(
        &cp,
        &admin,
        "cache_policies",
        json!({"name": "key-7c9", "scope": "api_key"}),
    )
    .await;
    assert_eq!(other.status, 201, "{other:?}");
    // An older gateway registers after both were saved.
    gw.beat("gw-old-7c9", OLD).await;
    // Keeping the team scope changes nothing an older gateway sees: an
    // operator can still edit (or switch off) a policy it already loads.
    let r = replace(
        &cp,
        &admin,
        &team,
        json!({"name": "team-7c9", "scope": "team", "enabled": false}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["doc"]["enabled"], false);
    let r = replace(
        &cp,
        &admin,
        &other,
        json!({"name": "key-7c9", "scope": "team"}),
    )
    .await;
    refused_at(&r, "/scope");
    let stored: Value = sqlx::query_scalar("SELECT doc FROM gw_resources WHERE id = $1::uuid")
        .bind(other.body["id"].as_str().expect("an id"))
        .fetch_one(cp.db().pool())
        .await
        .expect("read the other policy");
    assert_eq!(
        stored["scope"], "api_key",
        "the refused replace stored nothing"
    );
}

/// Fills `gw`'s credential to its instance cap: `online` rows heard from
/// just now at [`CURRENT`], and one more heard from `ago`, at `version`, which
/// a new instance then takes the place of.
async fn at_the_cap_with_a_quiet_instance(cp: &TestCp, gw: &Gateway, version: &str, ago: &str) {
    let online = aisix_control_plane::dp::heartbeat::MAX_INSTANCES - 1;
    sqlx::query(
        "INSERT INTO dp_instances (dp_id, instance_id, version) \
         SELECT $1::uuid, 'online-7ca-' || g, $2 FROM generate_series(1, $3) g",
    )
    .bind(&gw.dp_id)
    .bind(CURRENT)
    .bind(online)
    .execute(cp.db().pool())
    .await
    .expect("seed the online instances");
    sqlx::query(&format!(
        "INSERT INTO dp_instances (dp_id, instance_id, version, last_seen_at) \
         VALUES ($1::uuid, 'quiet-7ca', $2, now() - interval '{ago}')"
    ))
    .bind(&gw.dp_id)
    .bind(version)
    .execute(cp.db().pool())
    .await
    .expect("seed the quiet instance");
}

async fn cap_hit_at(cp: &TestCp, gw: &Gateway) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT instance_cap_hit_at FROM dp_credentials WHERE dp_id = $1::uuid")
        .bind(&gw.dp_id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the credential")
}

async fn instance_exists(cp: &TestCp, instance: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM dp_instances WHERE instance_id = $1)")
        .bind(instance)
        .fetch_one(cp.db().pool())
        .await
        .expect("look the instance up")
}

#[tokio::test]
async fn evicting_an_older_gateway_seen_within_a_day_hides_it_and_counts_as_a_cap_hit() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    at_the_cap_with_a_quiet_instance(&cp, &gw, OLD, "10 minutes").await;
    gw.beat("gw-new-7ca", CURRENT).await;
    assert!(
        !instance_exists(&cp, "quiet-7ca").await,
        "the quiet one was evicted"
    );
    assert!(cap_hit_at(&cp, &gw).await.is_some());
    // The older gateway is no longer in the list, and is still judged.
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7ca")).await;
    let message = refused_at(&r, "/scope");
    assert!(message.contains("实例上限"), "{message}");
}

#[tokio::test]
async fn evicting_a_current_gateway_or_one_gone_a_day_is_no_cap_hit() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    // A crash-looping current gateway leaves rows like this one behind.
    at_the_cap_with_a_quiet_instance(&cp, &gw, CURRENT, "10 minutes").await;
    gw.beat("gw-new-7cb", CURRENT).await;
    assert!(
        !instance_exists(&cp, "quiet-7ca").await,
        "the quiet one was evicted"
    );
    assert_eq!(cap_hit_at(&cp, &gw).await, None);
    // An older one the gate no longer judges anyway.
    sqlx::query(
        "INSERT INTO dp_instances (dp_id, instance_id, version, last_seen_at) \
         VALUES ($1::uuid, 'gone-7cb', $2, now() - interval '25 hours')",
    )
    .bind(&gw.dp_id)
    .bind(OLD)
    .execute(cp.db().pool())
    .await
    .expect("seed an instance gone a day");
    gw.beat("gw-newer-7cb", CURRENT).await;
    assert!(
        !instance_exists(&cp, "gone-7cb").await,
        "the day-old one was evicted"
    );
    assert_eq!(cap_hit_at(&cp, &gw).await, None);
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7cb")).await;
    assert_eq!(r.status, 201, "{r:?}");
}

/// Writes the api key `id` straight through `write_resource`, as the code
/// deriving keys does: a create, or a replace of the stored version.
async fn write_key(cp: &TestCp, id: Uuid, doc: Value, replace: bool) -> Result<i64, ApiError> {
    let ctx = WriteCtx::of(cp.state());
    cp.db()
        .tx(move |conn| {
            async move {
                let op = if replace {
                    WriteOp::Replace(&doc)
                } else {
                    WriteOp::Create(&doc)
                };
                let write = ResourceWrite {
                    kind: Kind::ApiKeys,
                    id,
                    op,
                    expected_version: None,
                    managed_by: Some("tenant:keys-7cc"),
                };
                Ok(write_resource(conn, &ctx, write).await?.version)
            }
            .boxed()
        })
        .await
}

#[tokio::test]
async fn re_enabling_a_key_an_older_gateway_would_serve_unrestricted_is_refused() {
    let Some((cp, _admin, gw)) = setup().await else {
        return;
    };
    gw.beat("gw-current-7cc", CURRENT).await;
    let key = |hash: &str, disabled: bool| {
        json!({
            "key_hash": hash,
            "allowed_model_ids": [],
            "allowed_provider_groups": ["shared"],
            "disabled": disabled,
        })
    };
    let (off, on) = (Uuid::new_v4(), Uuid::new_v4());
    write_key(&cp, off, key("hash-off-7cc", true), false)
        .await
        .expect("a disabled key, before any older gateway");
    write_key(&cp, on, key("hash-on-7cc", false), false)
        .await
        .expect("an enabled key, before any older gateway");
    gw.beat("gw-old-7cc", OLD).await;
    // An older gateway ignores the groups: switching the key back on would
    // let it reach every provider key there.
    let err = write_key(&cp, off, key("hash-off-7cc", false), true)
        .await
        .expect_err("re-enabling is judged again");
    assert_eq!((err.status.as_u16(), err.code), (422, "gateway_too_old"));
    assert_eq!(err.details[0].path, "/allowed_provider_groups");
    let err = write_key(
        &cp,
        off,
        json!({"key_hash": "hash-off-7cc", "allowed_model_ids": [],
               "allowed_provider_groups": ["shared"]}),
        true,
    )
    .await
    .expect_err("dropping the flag is re-enabling too");
    assert_eq!(err.code, "gateway_too_old");
    let stored: Value = sqlx::query_scalar("SELECT doc FROM gw_resources WHERE id = $1")
        .bind(off)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the key");
    assert_eq!(stored["disabled"], true, "still off");
    // Switching one off takes nothing new to an older gateway.
    write_key(&cp, on, key("hash-on-7cc", true), true)
        .await
        .expect("disabling is not refused");
}

#[tokio::test]
async fn a_key_that_gains_a_route_is_judged_again() {
    let Some((cp, _admin, gw)) = setup().await else {
        return;
    };
    gw.beat("gw-current-7cd", CURRENT).await;
    let key = |routes: Value| {
        json!({
            "key_hash": "hash-routes-7cd",
            "allowed_model_ids": [],
            "allowed_provider_groups": ["shared"],
            "allowed_routes": routes,
        })
    };
    let id = Uuid::new_v4();
    write_key(&cp, id, key(json!(["route-a-7cd"])), false)
        .await
        .expect("a key, before any older gateway");
    gw.beat("gw-old-7cd", OLD).await;
    // An older gateway would serve the new route through any provider key.
    let err = write_key(&cp, id, key(json!(["route-a-7cd", "route-b-7cd"])), true)
        .await
        .expect_err("a new route is judged again");
    assert_eq!((err.status.as_u16(), err.code), (422, "gateway_too_old"));
    assert_eq!(err.details[0].path, "/allowed_provider_groups");
    // Withdrawing one takes nothing new to it.
    write_key(&cp, id, key(json!([])), true)
        .await
        .expect("a route withdrawn is not refused");
}

#[tokio::test]
async fn an_api_key_with_endpoint_families_waits_for_the_gateways_that_read_them() {
    let Some((cp, _admin, gw)) = setup().await else {
        return;
    };
    gw.beat("gw-upgraded-7ce", OLD).await;
    let key = |hash: &str, families: Value| json!({"key_hash": hash, "allowed_model_ids": [], "allowed_endpoints": families});
    let id = Uuid::new_v4();
    // An older gateway ignores the families and lets the key call every
    // endpoint.
    let err = write_key(&cp, id, key("hash-7ce", json!(["chat_completions"])), false)
        .await
        .expect_err("refused while an older gateway is registered");
    assert_eq!((err.status.as_u16(), err.code), (422, "gateway_too_old"));
    assert_eq!(err.details[0].path, "/allowed_endpoints");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE id = $1")
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("count the key");
    assert_eq!(stored, 0, "nothing is stored");
    gw.beat("gw-upgraded-7ce", CURRENT).await;
    write_key(&cp, id, key("hash-7ce", json!(["chat_completions"])), false)
        .await
        .expect("accepted once every gateway reads the field");

    // An older gateway registers after the key was saved: a family added
    // is judged again, one withdrawn is not.
    gw.beat("gw-old-7ce", OLD).await;
    let err = write_key(
        &cp,
        id,
        key("hash-7ce", json!(["chat_completions", "embeddings"])),
        true,
    )
    .await
    .expect_err("a family added is judged again");
    assert_eq!(err.code, "gateway_too_old");
    assert_eq!(err.details[0].path, "/allowed_endpoints");
    write_key(&cp, id, key("hash-7ce", json!([])), true)
        .await
        .expect("a family withdrawn is not refused");
}

// 删除客户扩展的独立门槛会让旧开发构建立即获授已知但不隔离的族。
#[tokio::test]
async fn customer_endpoint_expansion_refuses_unproven_development_gateway_builds() {
    for cp_version in ["dev+sha-12ab345", "dev", "dev+sha-", "1.9.0+sha-12ab345"] {
        let Some(cp) = TestCp::start_stamped(cp_version).await else {
            return;
        };
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let gw = Gateway::issue(&cp, &admin).await;
        for gateway in [
            "dev+sha-f0f2360",
            "dev",
            "dev+sha-",
            "dev+sha-not-a-commit",
            "garbled",
            "1.8.2+sha-f0f2360",
        ] {
            gw.beat("customer-expansion-unproven-v9", gateway).await;
            for family in [
                "images",
                "audio",
                "videos",
                "rerank",
                "decisions",
                "realtime",
                "files",
                "batches",
                "fine_tuning",
                "moderations",
                "content_provenance_checks",
            ] {
                let id = Uuid::new_v4();
                let result = write_key(
                    &cp,
                    id,
                    json!({
                        "key_hash": format!("customer-unproven-{id}"),
                        "allowed_model_ids": [],
                        "allowed_endpoints": ["chat_completions", family],
                    }),
                    false,
                )
                .await;
                let error =
                    result.expect_err(&format!("{cp_version} must defer {family} on {gateway}"));
                assert_eq!(
                    (error.status.as_u16(), error.code),
                    (422, "gateway_too_old")
                );
                assert_eq!(error.details[0].path, "/allowed_endpoints");
                let stored: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE id = $1")
                        .bind(id)
                        .fetch_one(cp.db().pool())
                        .await
                        .expect("count refused key");
                assert_eq!(stored, 0, "a refused grant must not be stored");
            }
        }
    }
}

// 精确提交标识可证明同一构建；发行版则仍按本次发行的语义版本比较。
#[tokio::test]
async fn customer_endpoint_expansion_accepts_proven_builds_and_preserves_generic_dev_semantics() {
    for (cp_version, gateway) in [
        ("dev+sha-12ab345", "dev+sha-12ab345"),
        ("1.9.0+sha-12ab345", "1.9.0+sha-98cd765"),
        ("1.9.0+sha-12ab345", "1.10.0+sha-98cd765"),
    ] {
        let Some(cp) = TestCp::start_stamped(cp_version).await else {
            return;
        };
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let gw = Gateway::issue(&cp, &admin).await;
        gw.beat("customer-expansion-proven-v9", gateway).await;
        for family in ["files", "batches", "videos"] {
            let id = Uuid::new_v4();
            write_key(
                &cp,
                id,
                json!({
                    "key_hash": format!("customer-proven-{id}"),
                    "allowed_model_ids": [], "allowed_endpoints": [family],
                }),
                false,
            )
            .await
            .expect("a proven build can receive the expanded family");
        }
    }
    let Some(cp) = TestCp::start_stamped("dev+sha-12ab345").await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = Gateway::issue(&cp, &admin).await;
    gw.beat("customer-old-contract-v9", "dev+sha-f0f2360").await;
    let id = Uuid::new_v4();
    write_key(&cp, id, json!({
        "key_hash": "customer-old-contract-v9", "allowed_model_ids": [],
        "allowed_endpoints": ["chat_completions", "completions", "messages", "count_tokens", "responses", "embeddings", "models"],
    }), false).await.expect("the existing seven-family contract remains unchanged");
    let policy = create(
        &cp,
        &admin,
        "cache_policies",
        team_policy("customer-unrelated-v9"),
    )
    .await;
    assert_eq!(
        policy.status, 201,
        "the generic Dev contract stays in place: {policy:?}"
    );
}

// 不得把所有开发版本都当作同一构建，也不得把开发控制面当作已知发行门槛。
#[tokio::test]
async fn customer_endpoint_expansion_requires_a_real_shared_development_build_identity() {
    for (cp_version, gateway) in [
        ("dev", "dev"),
        ("dev+sha-", "dev+sha-"),
        ("dev+sha-not-a-commit", "dev+sha-not-a-commit"),
        ("dev+sha-12ab345", "1.99.0+sha-98cd765"),
        ("1.9.0+sha-12ab345", "dev+sha-12ab345"),
        ("1.9.0+sha-12ab345", "1.9.0-rc.1+sha-98cd765"),
    ] {
        let Some(cp) = TestCp::start_stamped(cp_version).await else {
            return;
        };
        let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
        let gw = Gateway::issue(&cp, &admin).await;
        gw.beat("customer-no-proof-v9", gateway).await;
        let id = Uuid::new_v4();
        let error = write_key(
            &cp,
            id,
            json!({
                "key_hash": format!("customer-no-proof-{id}"), "allowed_model_ids": [],
                "allowed_endpoints": ["files"],
            }),
            false,
        )
        .await
        .expect_err(&format!("{cp_version} cannot prove support on {gateway}"));
        assert_eq!(error.code, "gateway_too_old");
    }
}

// 保留族后重新启用也必须再判断；关闭和撤销族本身不能受阻。
#[tokio::test]
async fn customer_endpoint_expansion_preserves_narrowing_but_rechecks_reenable() {
    let Some(cp) = TestCp::start_stamped("dev+sha-12ab345").await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = Gateway::issue(&cp, &admin).await;
    gw.beat("customer-reenable-v9", "dev+sha-12ab345").await;
    let id = Uuid::new_v4();
    let key = |disabled: bool, families: Value| {
        json!({
            "key_hash": "customer-reenable-v9", "allowed_model_ids": [],
            "allowed_endpoints": families, "disabled": disabled,
        })
    };
    write_key(&cp, id, key(false, json!(["files", "batches"])), false)
        .await
        .expect("initial key");
    gw.beat("customer-reenable-v9", "dev+sha-f0f2360").await;
    write_key(&cp, id, key(true, json!(["files", "batches"])), true)
        .await
        .expect("disabling still works");
    let error = write_key(&cp, id, key(false, json!(["files", "batches"])), true)
        .await
        .expect_err("re-enabling must be judged again");
    assert_eq!(error.code, "gateway_too_old");
    write_key(&cp, id, key(true, json!(["files"])), true)
        .await
        .expect("withdrawing a family still works");
    let stored: Value = sqlx::query_scalar("SELECT doc FROM gw_resources WHERE id = $1")
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read narrowed key");
    assert_eq!(stored["disabled"], true);
    assert_eq!(stored["allowed_endpoints"], json!(["files"]));
}

// 实例上限导致清单不完整，即使已记录实例同构建也不能发布新权限。
#[tokio::test]
async fn customer_endpoint_expansion_refuses_an_incomplete_fleet_even_with_a_matching_build() {
    let Some(cp) = TestCp::start_stamped("dev+sha-12ab345").await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = Gateway::issue(&cp, &admin).await;
    gw.beat("customer-capped-v9", "dev+sha-12ab345").await;
    let changed =
        sqlx::query("UPDATE dp_credentials SET instance_cap_hit_at = now() WHERE dp_id = $1::uuid")
            .bind(&gw.dp_id)
            .execute(cp.db().pool())
            .await
            .expect("record an incomplete test fleet");
    assert_eq!(changed.rows_affected(), 1);
    let id = Uuid::new_v4();
    let error = write_key(&cp, id, json!({
        "key_hash": "customer-capped-v9", "allowed_model_ids": [], "allowed_endpoints": ["videos"],
    }), false).await.expect_err("the cap prevents proving all gateways support expansion");
    assert_eq!(error.code, "gateway_too_old");
}

// 离线实例在完整24小时窗口内仍算数，不能以在线状态或已撤销凭据绕过。
#[tokio::test]
async fn customer_endpoint_expansion_keeps_recent_offline_and_revoked_instances_in_the_fleet() {
    let Some(cp) = TestCp::start_stamped("dev+sha-12ab345").await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let old = Gateway::issue(&cp, &admin).await;
    old.beat("customer-old-window-v9", "dev+sha-f0f2360").await;
    age_instance(&cp, "customer-old-window-v9", "23 hours 59 minutes").await;
    let changed =
        sqlx::query("UPDATE dp_credentials SET revoked_at = now() WHERE dp_id = $1::uuid")
            .bind(&old.dp_id)
            .execute(cp.db().pool())
            .await
            .expect("revoke the test credential");
    assert_eq!(changed.rows_affected(), 1);
    let new = Gateway::issue(&cp, &admin).await;
    new.beat("customer-new-window-v9", "dev+sha-12ab345").await;
    let id = Uuid::new_v4();
    let doc = json!({"key_hash": "customer-window-v9", "allowed_model_ids": [], "allowed_endpoints": ["batches"]});
    let error = write_key(&cp, id, doc.clone(), false)
        .await
        .expect_err("the old heartbeat still counts");
    assert_eq!(error.code, "gateway_too_old");
    age_instance(&cp, "customer-old-window-v9", "24 hours 1 minute").await;
    write_key(&cp, id, doc, false)
        .await
        .expect("the honestly aged test fleet now supports expansion");
}

// 旧开发实例可被既有逻辑无标记淘汰；当前满额清单仍不能证明从未遗漏。
#[tokio::test]
async fn customer_endpoint_expansion_refuses_a_full_fleet_that_can_have_lost_an_unsafe_dev_row() {
    const BUILD: &str = "dev+sha-12ab345";
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = Gateway::issue(&cp, &admin).await;
    at_the_cap_with_a_quiet_instance(&cp, &gw, "dev+sha-f0f2360", "10 minutes").await;
    let online = aisix_control_plane::dp::heartbeat::MAX_INSTANCES - 1;
    let changed = sqlx::query("UPDATE dp_instances SET version = $2 WHERE dp_id = $1::uuid AND instance_id LIKE 'online-7ca-%'")
        .bind(&gw.dp_id).bind(BUILD).execute(cp.db().pool()).await.expect("stamp the real test instances");
    assert_eq!(changed.rows_affected(), online as u64);
    gw.beat("customer-replacement-v9", BUILD).await;
    assert!(
        !instance_exists(&cp, "quiet-7ca").await,
        "the old row was really evicted"
    );
    assert_eq!(
        cap_hit_at(&cp, &gw).await,
        None,
        "the existing generic Dev policy leaves no marker"
    );
    let versions: Vec<String> =
        sqlx::query_scalar("SELECT version FROM dp_instances WHERE dp_id = $1::uuid")
            .bind(&gw.dp_id)
            .fetch_all(cp.db().pool())
            .await
            .expect("read the remaining instances");
    assert_eq!(versions.len(), 128);
    assert!(
        versions.iter().all(|version| version == BUILD),
        "all visible instances now prove support"
    );
    let id = Uuid::new_v4();
    let error = write_key(&cp, id, json!({
        "key_hash": "customer-evicted-dev-v9", "allowed_model_ids": [], "allowed_endpoints": ["files"],
    }), false).await.expect_err("a full credential may have omitted an unsafe recent Dev instance");
    assert_eq!(error.code, "gateway_too_old");
}
// 缺省与 null 原本开放所有族；改成明确18族只收紧，不能误当新授权。
#[tokio::test]
async fn customer_endpoint_expansion_allows_an_existing_wildcard_to_narrow_to_explicit_families() {
    let Some(cp) = TestCp::start_stamped("dev+sha-12ab345").await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = Gateway::issue(&cp, &admin).await;
    let mut keys = Vec::new();
    for endpoints in [None, Some(Value::Null)] {
        let id = Uuid::new_v4();
        let mut before = json!({
            "key_hash": format!("customer-wildcard-{id}"),
            "allowed_model_ids": [], "disabled": false,
        });
        if let Some(endpoints) = endpoints {
            before["allowed_endpoints"] = endpoints;
        }
        write_key(&cp, id, before.clone(), false)
            .await
            .expect("an existing unrestricted key");
        keys.push((id, before));
    }
    gw.beat("customer-wildcard-old-v9", "dev+sha-f0f2360").await;
    for (id, before) in keys {
        let mut narrowed = before;
        narrowed["allowed_endpoints"] = json!([
            "chat_completions",
            "completions",
            "messages",
            "count_tokens",
            "responses",
            "embeddings",
            "models",
            "images",
            "audio",
            "videos",
            "rerank",
            "decisions",
            "realtime",
            "files",
            "batches",
            "fine_tuning",
            "moderations",
            "content_provenance_checks"
        ]);
        write_key(&cp, id, narrowed.clone(), true)
            .await
            .expect("restricting a wildcard to explicit families must save");
        let stored: Value = sqlx::query_scalar("SELECT doc FROM gw_resources WHERE id = $1")
            .bind(id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the restricted key");
        assert_eq!(stored, narrowed, "the complete restriction must be stored");
    }
}

// 关闭密钥、撤回透传路由和提前到期都应立即保存，即使同时写出明确族列表。
#[tokio::test]
async fn customer_endpoint_expansion_allows_disabling_and_other_narrowing_of_an_existing_wildcard()
{
    let Some(cp) = TestCp::start_stamped("dev+sha-12ab345").await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = Gateway::issue(&cp, &admin).await;
    let mut keys = Vec::new();
    for endpoints in [None, Some(Value::Null)] {
        let id = Uuid::new_v4();
        let mut before = json!({
            "key_hash": format!("customer-wildcard-close-{id}"), "allowed_model_ids": [],
            "allowed_routes": ["customer-route-withdrawn-v9"], "disabled": false,
            "expires_at": "2030-11-14T00:00:00Z",
        });
        if let Some(endpoints) = endpoints {
            before["allowed_endpoints"] = endpoints;
        }
        write_key(&cp, id, before.clone(), false)
            .await
            .expect("an existing unrestricted key");
        keys.push((id, before));
    }
    gw.beat("customer-wildcard-close-old-v9", "dev+sha-f0f2360")
        .await;
    for (id, before) in keys {
        let mut narrowed = before;
        narrowed["allowed_endpoints"] = json!(["files"]);
        narrowed["allowed_routes"] = json!([]);
        narrowed["disabled"] = json!(true);
        narrowed["expires_at"] = json!("2029-11-14T00:00:00Z");
        write_key(&cp, id, narrowed.clone(), true)
            .await
            .expect("a simultaneous close and restriction must save");
        let stored: Value = sqlx::query_scalar("SELECT doc FROM gw_resources WHERE id = $1")
            .bind(id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the closed key");
        assert_eq!(stored, narrowed, "all restrictions must be stored together");
    }
}

// 旧丢弃计数保护行可比被淘汰实例更早离开24小时窗口，满额判断须保留全部行。
#[tokio::test]
async fn customer_endpoint_expansion_counts_old_protected_rows_when_recent_rows_fall_below_the_cap()
{
    const BUILD: &str = "dev+sha-12ab345";
    let Some(cp) = TestCp::start_stamped(BUILD).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = Gateway::issue(&cp, &admin).await;
    at_the_cap_with_a_quiet_instance(&cp, &gw, "dev+sha-f0f2360", "10 minutes").await;
    let changed = sqlx::query("UPDATE dp_instances SET version = $2 WHERE dp_id = $1::uuid AND instance_id LIKE 'online-7ca-%'")
        .bind(&gw.dp_id).bind(BUILD).execute(cp.db().pool()).await.expect("stamp the retained test instances");
    assert_eq!(changed.rows_affected(), 127);
    let protected = sqlx::query(
        "UPDATE dp_instances SET last_seen_at = now() - interval '23 hours 59 minutes', usage_event_drops = $2 \
         WHERE dp_id = $1::uuid AND instance_id = 'online-7ca-1'",
    ).bind(&gw.dp_id).bind(json!({"sink_full": 1})).execute(cp.db().pool()).await.expect("seed the older protected report");
    assert_eq!(protected.rows_affected(), 1);
    gw.beat("customer-protected-replacement-v9", BUILD).await;
    assert!(
        !instance_exists(&cp, "quiet-7ca").await,
        "the unsafe recent Dev row was evicted"
    );
    assert!(
        instance_exists(&cp, "online-7ca-1").await,
        "the older drop report was retained"
    );
    assert_eq!(
        cap_hit_at(&cp, &gw).await,
        None,
        "generic Dev eviction leaves no marker"
    );
    age_instance(&cp, "online-7ca-1", "25 hours").await;
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE last_seen_at > now() - interval '24 hours') \
         FROM dp_instances WHERE dp_id = $1::uuid",
    )
    .bind(&gw.dp_id)
    .fetch_one(cp.db().pool())
    .await
    .expect("count all and recent reports");
    assert_eq!(
        counts,
        (128, 127),
        "the recent-only count would miss the full credential"
    );
    let id = Uuid::new_v4();
    let error = write_key(&cp, id, json!({
        "key_hash": "customer-protected-cap-v9", "allowed_model_ids": [], "allowed_endpoints": ["batches"],
    }), false).await.expect_err("old protected rows still require the conservative capacity gate");
    assert_eq!(error.code, "gateway_too_old");
}
