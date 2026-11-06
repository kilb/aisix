//! Rollout status: how far each stored document has reached the online
//! gateways, as their heartbeats report it, per document
//! (`ResourceView.rollout`) and for the whole environment
//! (`GET /admin/rollout`).
//!
//! The documents are projected to the test kine by the real projector, and
//! the revision each one reached kine at is read from the API; gateways are
//! registered and send heartbeats the way real ones do.

mod support;

use std::time::{Duration, Instant};

use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::projector::{PROJECTION_BLOCKED, PROJECTION_STALLED, RECONCILE_BLOCKED};
use aisix_control_plane::settings;
use serde_json::{json, Value};
use support::{DpClient, OperatorSession, TestCp};
use uuid::Uuid;

/// Long enough for a projector that is backing off at its 10 s ceiling.
const PATIENCE: Duration = Duration::from_secs(30);

struct Gateway {
    dp_id: String,
    client: DpClient,
}

impl Gateway {
    async fn issue(cp: &TestCp, admin: &OperatorSession) -> Gateway {
        let bundle = cp.issue_bundle(admin, "gw-rollout-7d1").await;
        Gateway {
            client: cp.dp_client(&bundle),
            dp_id: bundle.dp_id,
        }
    }

    /// A heartbeat of `instance` that has applied up to `revision` and
    /// reports `rejected` as its `rejected_resources`.
    async fn beat(&self, instance: &str, revision: i64, rejected: Value) {
        let r = self
            .client
            .heartbeat(json!({
                "dp_id": self.dp_id,
                "instance_id": instance,
                "hostname": format!("host-{instance}"),
                "uptime_seconds": 1,
                "version": "1.9.0+sha-gw7d1",
                "supported_guardrail_kinds": [],
                "applied_revision": revision,
                "exporter_health": [],
                "rejected_resources": rejected,
            }))
            .await;
        assert_eq!(r.status, 200, "beat {instance} at {revision}: {r:?}");
    }
}

/// One rejection as a gateway reports it, for the document at `key`.
fn rejection(key: &str, error: &str) -> Value {
    json!({
        "key": key,
        "kind": "schema_failed",
        "error": error,
        "timestamp_unix_secs": 1_770_000_000u64,
    })
}

async fn create_policy(cp: &TestCp, admin: &OperatorSession, name: &str) -> String {
    let r = cp
        .send(
            "POST",
            "/admin/resources/cache_policies",
            Some(admin.cookie()),
            Some(json!({"doc": {"name": name}})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        r.body["rollout"]["state"], "pending",
        "not projected yet: {r:?}"
    );
    r.body["id"].as_str().expect("an id").to_string()
}

async fn rollout_of(cp: &TestCp, admin: &OperatorSession, id: &str) -> Value {
    let r = cp
        .get_with_cookie(
            &format!("/admin/resources/cache_policies/{id}"),
            admin.cookie(),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body["rollout"].clone()
}

/// The document's rollout once the projector has written it to kine.
async fn projected(cp: &TestCp, admin: &OperatorSession, id: &str) -> Value {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let rollout = rollout_of(cp, admin, id).await;
        if rollout["state"] != "pending" {
            return rollout;
        }
        assert!(Instant::now() < deadline, "never projected: {rollout}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn summary(cp: &TestCp, admin: &OperatorSession) -> Value {
    let r = cp.get_with_cookie("/admin/rollout", admin.cookie()).await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body
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
async fn a_resource_view_reports_the_rollout_the_heartbeats_imply() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = create_policy(&cp, &admin, "rollout-7d1").await;
    let rollout = projected(&cp, &admin, &id).await;
    assert_eq!(rollout["state"], "no_gateways", "{rollout}");
    let revision = rollout["kine_revision"].as_i64().expect("a kine revision");

    let gw = Gateway::issue(&cp, &admin).await;
    gw.beat("gw-a-7d1", revision - 1, json!([])).await;
    let rollout = rollout_of(&cp, &admin, &id).await;
    assert_eq!(
        (&rollout["state"], &rollout["applied"], &rollout["online"]),
        (&json!("propagating"), &json!(0), &json!(1)),
        "{rollout}"
    );

    gw.beat("gw-a-7d1", revision, json!([])).await;
    let rollout = rollout_of(&cp, &admin, &id).await;
    assert_eq!(
        (&rollout["state"], &rollout["applied"], &rollout["online"]),
        (&json!("applied"), &json!(1), &json!(1)),
        "{rollout}"
    );
    assert_eq!(rollout["kine_revision"], revision);

    // A second gateway behind the write holds it back; once offline, no more.
    gw.beat("gw-b-7d1", revision - 1, json!([])).await;
    let rollout = rollout_of(&cp, &admin, &id).await;
    assert_eq!(
        (&rollout["state"], &rollout["applied"], &rollout["online"]),
        (&json!("propagating"), &json!(1), &json!(2)),
        "{rollout}"
    );
    age_instance(&cp, "gw-b-7d1", "10 minutes").await;
    let rollout = rollout_of(&cp, &admin, &id).await;
    assert_eq!(
        (&rollout["state"], &rollout["online"]),
        (&json!("applied"), &json!(1)),
        "{rollout}"
    );

    // A gateway that has the write and refused it.
    let key = format!("{}cache_policies/{id}", cp.env().env_prefix());
    gw.beat(
        "gw-a-7d1",
        revision,
        json!([rejection(&key, "bad policy 7d1")]),
    )
    .await;
    let rollout = rollout_of(&cp, &admin, &id).await;
    assert_eq!(rollout["state"], "rejected", "{rollout}");
    assert_eq!(
        rollout["rejections"],
        json!([{"instance_id": "gw-a-7d1", "hostname": "host-gw-a-7d1", "error": "bad policy 7d1"}])
    );

    // The list carries the same rollout.
    let r = cp
        .get_with_cookie("/admin/resources/cache_policies", admin.cookie())
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let listed = r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|v| v["id"] == id.as_str())
        .expect("the policy is listed")
        .clone();
    assert_eq!(listed["rollout"]["state"], "rejected", "{listed}");
}

#[tokio::test]
async fn the_rollout_summary_lists_rejected_resources_by_name() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let refused = create_policy(&cp, &admin, "refused-7d2").await;
    let fine = create_policy(&cp, &admin, "fine-7d2").await;
    let mut revision = 0;
    for id in [&refused, &fine] {
        let r = projected(&cp, &admin, id).await;
        revision = revision.max(r["kine_revision"].as_i64().expect("a kine revision"));
    }
    let gw = Gateway::issue(&cp, &admin).await;
    let key = format!("{}cache_policies/{refused}", cp.env().env_prefix());
    gw.beat(
        "gw-a-7d2",
        revision,
        json!([rejection(&key, "bad policy 7d2")]),
    )
    .await;
    let s = summary(&cp, &admin).await;
    assert_eq!(s["online_instances"], 1, "{s}");
    assert_eq!(
        (&s["pending"], &s["propagating"]),
        (&json!(0), &json!(0)),
        "{s}"
    );
    assert_eq!(s["reconcile_blocked"], false, "{s}");
    assert_eq!(
        s["rejected"],
        json!([{
            "kind": "cache_policies",
            "id": refused,
            "name": "refused-7d2",
            "errors": [{"instance_id": "gw-a-7d2", "hostname": "host-gw-a-7d2", "error": "bad policy 7d2"}],
        }]),
        "{s}"
    );
    assert_eq!(s["unsafe"], json!([]), "{s}");

    // A gateway that has applied nothing yet is behind every document,
    // the platform tenant's own team among them.
    gw.beat("gw-b-7d2", 0, json!([])).await;
    let documents: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_resources")
        .fetch_one(cp.db().pool())
        .await
        .expect("count documents");
    let platform_team: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_resources r JOIN tenants t ON r.managed_by = 'tenant:' || t.id \
         WHERE t.is_internal AND r.kind = 'teams'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("count the platform tenant's team");
    assert_eq!(platform_team, 1);
    let s = summary(&cp, &admin).await;
    assert_eq!(s["online_instances"], 2, "{s}");
    // The refused one is still refused by the gateway that has it.
    assert_eq!(s["propagating"], documents - 1, "{s}");
    assert_eq!(s["rejected"][0]["id"], refused.as_str(), "{s}");
}

#[tokio::test]
async fn a_tenant_view_reports_the_rollout_of_its_documents() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let r = cp
        .send(
            "POST",
            "/admin/tenants",
            Some(admin.cookie()),
            Some(json!({"name": "tenant-7d5"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let tenant = r.body["id"].as_str().expect("an id").to_string();
    let tenant_rollout = || {
        let (cp, admin, tenant) = (&cp, &admin, &tenant);
        async move {
            let r = cp
                .get_with_cookie(&format!("/admin/tenants/{tenant}"), admin.cookie())
                .await;
            assert_eq!(r.status, 200, "{r:?}");
            r.body["rollout"].clone()
        }
    };
    // Written after the tenant's documents, so at a later revision than
    // any of them.
    let later = create_policy(&cp, &admin, "after-7d5").await;
    let revision = projected(&cp, &admin, &later).await["kine_revision"]
        .as_i64()
        .expect("a kine revision");
    assert_eq!(tenant_rollout().await["state"], "no_gateways");
    let gw = Gateway::issue(&cp, &admin).await;
    gw.beat("gw-a-7d5", 0, json!([])).await;
    let rollout = tenant_rollout().await;
    assert_eq!(
        (&rollout["state"], &rollout["online"]),
        (&json!("propagating"), &json!(1)),
        "{rollout}"
    );
    gw.beat("gw-a-7d5", revision, json!([])).await;
    let rollout = tenant_rollout().await;
    assert_eq!(
        (&rollout["state"], &rollout["applied"], &rollout["online"]),
        (&json!("applied"), &json!(1), &json!(1)),
        "{rollout}"
    );
}

#[tokio::test]
async fn while_a_credential_recently_hit_its_instance_cap_nothing_reads_as_applied() {
    let Some(cp) = TestCp::start_with_kine().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let id = create_policy(&cp, &admin, "capped-7d3").await;
    let revision = projected(&cp, &admin, &id).await["kine_revision"]
        .as_i64()
        .expect("a kine revision");
    let gw = Gateway::issue(&cp, &admin).await;
    gw.beat("gw-a-7d3", revision, json!([])).await;
    assert_eq!(rollout_of(&cp, &admin, &id).await["state"], "applied");
    assert_eq!(
        summary(&cp, &admin).await["instance_cap_hit_at"],
        Value::Null
    );

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
    // Instances were turned away: some gateway this one cannot see may not
    // have it.
    hit("1 hour").await;
    assert_eq!(rollout_of(&cp, &admin, &id).await["state"], "propagating");
    let s = summary(&cp, &admin).await;
    assert!(s["instance_cap_hit_at"].is_string(), "{s}");
    assert!(s["propagating"].as_i64().expect("a count") >= 1, "{s}");
    hit("25 hours").await;
    assert_eq!(rollout_of(&cp, &admin, &id).await["state"], "applied");
    assert_eq!(
        summary(&cp, &admin).await["instance_cap_hit_at"],
        Value::Null
    );
}

#[tokio::test]
async fn a_delete_kine_had_no_key_for_does_not_stay_propagating() {
    // No kine: the projector never runs, so the rows below stay as written.
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let insert = |deleted: bool, kine_revision: Option<i64>, name: &'static str| {
        let cp = &cp;
        async move {
            sqlx::query(
                "INSERT INTO gw_resources (kind, id, doc, identity, deleted, version, \
                   projected_version, kine_revision) \
                 VALUES ('cache_policies', $1, $2, $3, $4, 2, 2, $5)",
            )
            .bind(Uuid::new_v4())
            .bind(json!({ "name": name }))
            .bind(name)
            .bind(deleted)
            .bind(kine_revision)
            .execute(cp.db().pool())
            .await
            .expect("insert a projected row");
        }
    };
    // A delete projected when kine no longer held the key: kine has no
    // revision for it, and no gateway's ever reaches one.
    insert(true, None, "gone-7d4").await;
    // A live document written to kine after everything the gateway has.
    insert(false, Some(50), "ahead-7d4").await;
    let gw = Gateway::issue(&cp, &admin).await;
    gw.beat("gw-a-7d4", 10, json!([])).await;
    let s = summary(&cp, &admin).await;
    assert_eq!(s["propagating"], 1, "only the live document is behind: {s}");
}

#[tokio::test]
async fn the_rollout_summary_names_a_stopped_projection_by_ids_only() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let s = summary(&cp, &viewer).await;
    assert_eq!(
        (
            &s["projection_stalled"],
            &s["projection_blocked"],
            &s["reconcile_blocked"]
        ),
        (&Value::Null, &Value::Null, &json!(false)),
        "{s}"
    );
    assert_eq!(s["online_instances"], 0, "{s}");

    let stalled_id = Uuid::new_v4();
    let (owner, mine) = (Uuid::new_v4(), Uuid::new_v4());
    let mut conn = cp.db().pool().acquire().await.expect("a connection");
    settings::set(
        &mut conn,
        PROJECTION_STALLED,
        &json!({"kind": "provider_keys", "id": stalled_id, "seq": 41,
                "error": "a stored credential does not open under the master key"}),
    )
    .await
    .expect("record a stall");
    settings::set(
        &mut conn,
        PROJECTION_BLOCKED,
        &json!({"reason": "another control plane owns this environment",
                "owner_database_id": owner, "database_id": mine}),
    )
    .await
    .expect("record a block");
    settings::set(&mut conn, RECONCILE_BLOCKED, &json!(true))
        .await
        .expect("record a blocked reconcile");
    drop(conn);
    let s = summary(&cp, &viewer).await;
    assert_eq!(
        s["projection_stalled"],
        json!({"kind": "provider_keys", "id": stalled_id,
               "error": "a stored credential does not open under the master key"}),
        "{s}"
    );
    assert_eq!(
        s["projection_blocked"],
        json!({"owner_database_id": owner, "database_id": mine}),
        "{s}"
    );
    assert_eq!(s["reconcile_blocked"], true, "{s}");
}
