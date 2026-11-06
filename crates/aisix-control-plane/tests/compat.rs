//! The save-time gateway-version gate: a document carrying a field that
//! gateways older than its minimum version would misread is refused while
//! such a gateway has reported in the last 24 hours, and while the control
//! plane cannot tell which versions its gateways run.
//!
//! Gateways are registered the way real ones are: a credential is issued and
//! its gateway sends heartbeats (`support::DpClient`).

mod support;

use aisix_control_plane::auth::OperatorRole;
use serde_json::{json, Value};
use support::{DpClient, OperatorSession, Resp, TestCp};

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
        json!({"name": "env-7c1", "scope": "env"}),
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
async fn a_gated_write_is_refused_while_no_gateway_is_online() {
    let Some((cp, admin, gw)) = setup().await else {
        return;
    };
    // No gateway has reported at all: which version will load it is unknown.
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c5")).await;
    let message = refused_at(&r, "/scope");
    assert!(message.contains("没有在线的网关实例"), "{message}");
    // One reported recently enough to count, but is not online now.
    gw.beat("gw-quiet-7c5", CURRENT).await;
    age_instance(&cp, "gw-quiet-7c5", "10 minutes").await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c5")).await;
    refused_at(&r, "/scope");
    assert_eq!(cache_policy_rows(&cp).await, 0, "nothing is stored");
    gw.beat("gw-quiet-7c5", CURRENT).await;
    let r = create(&cp, &admin, "cache_policies", team_policy("team-7c5")).await;
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
    let env = create(
        &cp,
        &admin,
        "cache_policies",
        json!({"name": "env-7c9", "scope": "env"}),
    )
    .await;
    assert_eq!(env.status, 201, "{env:?}");
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
        &env,
        json!({"name": "env-7c9", "scope": "team"}),
    )
    .await;
    refused_at(&r, "/scope");
    let stored: Value = sqlx::query_scalar("SELECT doc FROM gw_resources WHERE id = $1::uuid")
        .bind(env.body["id"].as_str().expect("an id"))
        .fetch_one(cp.db().pool())
        .await
        .expect("read the env policy");
    assert_eq!(stored["scope"], "env", "the refused replace stored nothing");
}
