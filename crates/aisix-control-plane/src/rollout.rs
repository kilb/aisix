//! How far a stored document has reached the gateways (`Rollout` in the
//! API), the summary of several documents that a tenant view carries, and
//! the environment's summary, `GET /admin/rollout`.
//!
//! A document's rollout compares the kine revision the projector recorded
//! for its latest version (`gw_resources.kine_revision`) with the
//! `applied_revision` each online gateway reports in its heartbeat: a
//! gateway's configuration is consistent as of that revision, so a gateway
//! at or past a document's revision has dealt with it, and a rejection it
//! reports for the document is about that version. Gateways not heard from
//! within `CP_INSTANCE_STALE_SECS` never count.
//!
//! Two cases need a rule of their own:
//! - A delete projected while kine no longer held the key (it was never
//!   written there, or the answer to an earlier delete was lost) has no
//!   revision. Kine recorded nothing for it, so nothing is behind it: once
//!   projected it is `applied` on every online gateway. A gateway's
//!   revision moves only with the keys it watches, so waiting for any
//!   other revision, such as kine's current one, could wait forever.
//! - While a credential has turned a new instance away within
//!   [`FLEET_WINDOW`], the instances seen are not the whole fleet, and a
//!   document they all have may still be missing from one not seen: nothing
//!   reads as `applied` then, only as `propagating`.

use axum::extract::State;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::compat::{FIELD_FLOORS, FLEET_WINDOW};
use crate::db::DbError;
use crate::error::ApiError;
use crate::gateways::stale_window_secs;
use crate::projector::{PROJECTION_BLOCKED, PROJECTION_STALLED, RECONCILE_BLOCKED};
use crate::resources::store::ResourceRow;
use crate::resources::Kind;
use crate::settings::{self, EnvSettings};
use crate::AppState;

/// The fixed root under which a gateway names a guardrail whose runtime
/// build failed, whatever the deployment's prefix
/// (`crates/aisix-guardrails/src/build.rs`, `record_build_rejections`).
const RUNTIME_GUARDRAILS: &str = "/aisix/runtime/guardrails/";

/// A document's rollout state, as the API names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutState {
    Pending,
    Propagating,
    Applied,
    Rejected,
    NoGateways,
}

impl RolloutState {
    /// How bad the state is when several documents are summarised: a
    /// rejection outranks everything, then a document the gateways cannot
    /// have yet, then one some gateway is still behind on.
    fn severity(self) -> u8 {
        match self {
            RolloutState::Applied => 0,
            RolloutState::NoGateways => 1,
            RolloutState::Propagating => 2,
            RolloutState::Pending => 3,
            RolloutState::Rejected => 4,
        }
    }
}

/// `Rollout = {state, kine_revision, applied, online, rejections}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceRollout {
    pub state: RolloutState,
    pub kine_revision: Option<i64>,
    /// Online gateways that have applied the document.
    pub applied: u64,
    pub online: u64,
    /// `{instance_id, hostname, error}` for each gateway that refused it.
    pub rejections: Vec<Value>,
}

/// What a document's rollout is computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutInput {
    pub kind: Kind,
    pub id: Uuid,
    /// Whether the projector has written the latest version to kine.
    pub projected: bool,
    /// The kine revision of the last projection (of an earlier version while
    /// `projected` is false); `None` for a row never projected, and for a
    /// delete projected while kine held no key.
    pub kine_revision: Option<i64>,
}

impl RolloutInput {
    pub fn of_row(row: &ResourceRow) -> RolloutInput {
        RolloutInput {
            kind: row.kind,
            id: row.id,
            projected: row.projected_version == Some(row.version),
            kine_revision: row.kine_revision,
        }
    }
}

/// A gateway instance, as its latest heartbeat reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceView {
    pub instance_id: String,
    pub hostname: String,
    /// Heard from within `CP_INSTANCE_STALE_SECS`.
    pub online: bool,
    pub applied_revision: i64,
    /// `(kind, id, error)` of each document of this environment that the
    /// instance reports it refused.
    pub rejections: Vec<(Kind, String, String)>,
}

/// The rollout of `r` over `instances`: `pending` until projected,
/// `no_gateways` with none online, `rejected` when an online instance at or
/// past its revision refuses it, `propagating` while an online instance is
/// behind it, and `applied` otherwise (see the module docs for a delete
/// without a revision). Offline instances never count.
pub fn compute(r: &RolloutInput, instances: &[InstanceView]) -> ResourceRollout {
    let online: Vec<&InstanceView> = instances.iter().filter(|i| i.online).collect();
    let mut out = ResourceRollout {
        state: RolloutState::Pending,
        kine_revision: r.kine_revision,
        applied: 0,
        online: online.len() as u64,
        rejections: Vec::new(),
    };
    if !r.projected {
        return out;
    }
    if online.is_empty() {
        out.state = RolloutState::NoGateways;
        return out;
    }
    let Some(revision) = r.kine_revision else {
        out.state = RolloutState::Applied;
        out.applied = out.online;
        return out;
    };
    let id = r.id.to_string();
    let mut behind = false;
    for instance in online {
        if instance.applied_revision < revision {
            behind = true;
            continue;
        }
        let refused = instance
            .rejections
            .iter()
            .find(|(kind, rid, _)| *kind == r.kind && *rid == id);
        match refused {
            Some((_, _, error)) => out.rejections.push(json!({
                "instance_id": instance.instance_id,
                "hostname": instance.hostname,
                "error": error,
            })),
            None => out.applied += 1,
        }
    }
    out.state = if !out.rejections.is_empty() {
        RolloutState::Rejected
    } else if behind {
        RolloutState::Propagating
    } else {
        RolloutState::Applied
    };
    out
}

/// The gateway instances, and whether their view is complete.
#[derive(Debug, Clone)]
pub struct Fleet {
    pub instances: Vec<InstanceView>,
    /// The latest time, within [`FLEET_WINDOW`], that a credential turned a
    /// new instance away (`dp_credentials.instance_cap_hit_at`); `instances`
    /// may then miss live ones.
    pub cap_hit_at: Option<DateTime<Utc>>,
}

impl Fleet {
    /// Every instance that has reported, online or not (offline ones are
    /// pruned after a week), with `stale_secs` as the online window.
    pub async fn load(
        conn: &mut PgConnection,
        env: &EnvSettings,
        stale_secs: f64,
    ) -> Result<Fleet, DbError> {
        let rows: Vec<(String, String, bool, i64, Value)> = sqlx::query_as(
            "SELECT instance_id, hostname, last_seen_at > now() - make_interval(secs => $1), \
               applied_revision, rejected_resources \
             FROM dp_instances ORDER BY first_seen_at, dp_id, instance_id",
        )
        .bind(stale_secs)
        .fetch_all(&mut *conn)
        .await?;
        let cap_hit_at: Option<DateTime<Utc>> = sqlx::query_scalar(&format!(
            "SELECT max(instance_cap_hit_at) FROM dp_credentials \
             WHERE instance_cap_hit_at > now() - {FLEET_WINDOW}"
        ))
        .fetch_one(&mut *conn)
        .await?;
        let instances = rows
            .into_iter()
            .map(
                |(instance_id, hostname, online, applied_revision, rejected)| InstanceView {
                    instance_id,
                    hostname,
                    online,
                    applied_revision,
                    rejections: rejections_of(env, &rejected),
                },
            )
            .collect();
        Ok(Fleet {
            instances,
            cap_hit_at,
        })
    }

    /// [`compute`] over these instances, except that nothing is `applied`
    /// while the view may miss instances: it is `propagating` then.
    pub fn rollout(&self, r: &RolloutInput) -> ResourceRollout {
        let mut out = compute(r, &self.instances);
        if self.cap_hit_at.is_some() && out.state == RolloutState::Applied {
            out.state = RolloutState::Propagating;
        }
        out
    }

    fn online(&self) -> usize {
        self.instances.iter().filter(|i| i.online).count()
    }
}

/// The documents of this environment that a heartbeat's
/// `rejected_resources` names, with the gateway's error text.
fn rejections_of(env: &EnvSettings, reported: &Value) -> Vec<(Kind, String, String)> {
    reported
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let (kind, id) = resource_of_rejected_key(env, r["key"].as_str()?)?;
            let error = r["error"].as_str().unwrap_or_default().to_string();
            Some((kind, id, error))
        })
        .collect()
}

/// The document a rejected key names: `<env_prefix><segment>/<id>` for a
/// document of this environment, or `/aisix/runtime/guardrails/<id>` for a
/// guardrail whose runtime build failed. `None` for any other key.
pub fn resource_of_rejected_key(env: &EnvSettings, key: &str) -> Option<(Kind, String)> {
    let (kind, id) = match key.strip_prefix(&env.env_prefix()) {
        Some(rest) => {
            let (segment, id) = rest.split_once('/')?;
            (Kind::from_segment(segment)?, id)
        }
        None => (Kind::Guardrails, key.strip_prefix(RUNTIME_GUARDRAILS)?),
    };
    if id.is_empty() || id.contains('/') {
        return None;
    }
    Some((kind, id.to_string()))
}

/// The summary of `states`: the worst of them (the first, among equals),
/// with every document's rejections, the fewest gateways any document has
/// reached, and the most gateways seen online. Nothing to summarise is
/// nothing left to roll out: `applied`.
pub fn worst(states: &[ResourceRollout]) -> ResourceRollout {
    let Some(first) = states.first() else {
        return ResourceRollout {
            state: RolloutState::Applied,
            kine_revision: None,
            applied: 0,
            online: 0,
            rejections: Vec::new(),
        };
    };
    let mut summary = states
        .iter()
        .fold(first, |worst, s| {
            if s.state.severity() > worst.state.severity() {
                s
            } else {
                worst
            }
        })
        .clone();
    summary.applied = states.iter().map(|s| s.applied).min().unwrap_or(0);
    summary.online = states.iter().map(|s| s.online).max().unwrap_or(0);
    summary.rejections = states
        .iter()
        .flat_map(|s| s.rejections.iter().cloned())
        .collect();
    summary
}

/// `GET /admin/rollout`: the environment's rollout. Counts are over every
/// stored document, the platform tenant's own included, and a delete
/// counts until the gateways have it. Projection problems are named by ids
/// only, never by a document: `projection_stalled` is the document
/// projection stopped at (`kind`, `id`, and the projector's own error
/// text), `projection_blocked` the database that owns this environment
/// instead and this one (what `POST /admin/rollout/ownership` takes).
/// `partially_compatible` is what each instance heard from within
/// [`FLEET_WINDOW`] reports it loaded with fields ignored, and `unsafe` the
/// ones among those fields that a gateway-version floor guards
/// ([`FIELD_FLOORS`]): such a gateway serves the document without
/// enforcing the field. `instance_cap_hit_at` is set while the instances
/// seen may not be the whole fleet.
pub async fn summary(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let fleet = Fleet::load(&mut conn, &state.env, stale_window_secs(&state.cfg)).await?;
    /// `(kind, id, name, projected, kine_revision)` of a stored document.
    type DocumentState = (String, Uuid, Option<String>, bool, Option<i64>);
    let rows: Vec<DocumentState> = sqlx::query_as(
        "SELECT kind, id, COALESCE(doc->>'display_name', doc->>'name'), \
           projected_version IS NOT DISTINCT FROM version, kine_revision \
         FROM gw_resources ORDER BY seq",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let (mut pending, mut propagating, mut rejected) = (0u64, 0u64, Vec::new());
    for (segment, id, name, projected, kine_revision) in rows {
        let Some(kind) = Kind::from_segment(&segment) else {
            continue;
        };
        let rollout = fleet.rollout(&RolloutInput {
            kind,
            id,
            projected,
            kine_revision,
        });
        match rollout.state {
            RolloutState::Pending => pending += 1,
            RolloutState::Propagating => propagating += 1,
            RolloutState::Rejected => rejected.push(json!({
                "kind": kind,
                "id": id,
                "name": name,
                "errors": rollout.rejections,
            })),
            RolloutState::Applied | RolloutState::NoGateways => {}
        }
    }
    let reports: Vec<(String, String, Value)> = sqlx::query_as(&format!(
        "SELECT instance_id, hostname, partially_compatible_resources FROM dp_instances \
         WHERE last_seen_at > now() - {FLEET_WINDOW} ORDER BY first_seen_at, dp_id, instance_id"
    ))
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let (mut partially_compatible, mut ignoring_floored) = (Vec::new(), Vec::new());
    for (instance_id, hostname, entries) in reports {
        for entry in entries.as_array().into_iter().flatten() {
            let (Some(kind), Some(field)) = (entry["kind"].as_str(), entry["field"].as_str())
            else {
                continue;
            };
            partially_compatible.push(json!({
                "instance_id": instance_id,
                "kind": kind,
                "field": field,
                "count": entry["count"],
            }));
            let floored = FIELD_FLOORS
                .iter()
                .any(|f| f.kind.segment() == kind && f.pointer.strip_prefix('/') == Some(field));
            if floored {
                ignoring_floored.push(json!({
                    "instance_id": instance_id,
                    "hostname": hostname,
                    "kind": kind,
                    "field": field,
                }));
            }
        }
    }
    let reconcile_blocked = settings::get(&mut conn, RECONCILE_BLOCKED)
        .await?
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let stalled = settings::get(&mut conn, PROJECTION_STALLED)
        .await?
        .map(|v| json!({"kind": v["kind"], "id": v["id"], "error": v["error"]}));
    let blocked = settings::get(&mut conn, PROJECTION_BLOCKED).await?.map(
        |v| json!({"owner_database_id": v["owner_database_id"], "database_id": v["database_id"]}),
    );
    Ok(Json(json!({
        "online_instances": fleet.online(),
        "pending": pending,
        "propagating": propagating,
        "reconcile_blocked": reconcile_blocked,
        "rejected": rejected,
        "partially_compatible": partially_compatible,
        "unsafe": ignoring_floored,
        "projection_stalled": stalled,
        "projection_blocked": blocked,
        "instance_cap_hit_at": fleet.cap_hit_at,
    })))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn at(state: RolloutState, applied: u64, online: u64) -> ResourceRollout {
        ResourceRollout {
            state,
            kine_revision: Some(applied as i64),
            applied,
            online,
            rejections: Vec::new(),
        }
    }

    #[test]
    fn the_worst_state_wins_whatever_the_order() {
        use RolloutState::*;
        let ranked = [Applied, NoGateways, Propagating, Pending, Rejected];
        for (i, worse) in ranked.iter().enumerate() {
            for better in &ranked[..i] {
                for pair in [
                    [at(*better, 1, 1), at(*worse, 1, 1)],
                    [at(*worse, 1, 1), at(*better, 1, 1)],
                ] {
                    assert_eq!(worst(&pair).state, *worse, "{better:?} vs {worse:?}");
                }
            }
        }
    }

    #[test]
    fn a_summary_keeps_every_rejection_and_the_least_progress() {
        let mut rejected = at(RolloutState::Rejected, 3, 4);
        rejected.rejections = vec![json!({"instance_id": "gw-a-7c1"})];
        let mut also = at(RolloutState::Rejected, 2, 4);
        also.rejections = vec![json!({"instance_id": "gw-b-7c1"})];
        let summary = worst(&[at(RolloutState::Applied, 4, 4), rejected, also]);
        assert_eq!(summary.state, RolloutState::Rejected);
        assert_eq!(summary.kine_revision, Some(3), "the first of the worst");
        assert_eq!((summary.applied, summary.online), (2, 4));
        assert_eq!(
            summary.rejections,
            [
                json!({"instance_id": "gw-a-7c1"}),
                json!({"instance_id": "gw-b-7c1"})
            ]
        );
        assert_eq!(worst(&[]).state, RolloutState::Applied);
    }

    fn written(revision: i64) -> RolloutInput {
        RolloutInput {
            kind: Kind::Models,
            id: Uuid::from_u128(0x7e1),
            projected: true,
            kine_revision: Some(revision),
        }
    }

    fn instance(name: &str, online: bool, applied_revision: i64) -> InstanceView {
        InstanceView {
            instance_id: name.to_string(),
            hostname: format!("host-{name}"),
            online,
            applied_revision,
            rejections: Vec::new(),
        }
    }

    fn rejecting(mut i: InstanceView, r: &RolloutInput, error: &str) -> InstanceView {
        i.rejections
            .push((r.kind, r.id.to_string(), error.to_string()));
        i
    }

    fn summary(r: &ResourceRollout) -> (RolloutState, u64, u64) {
        (r.state, r.applied, r.online)
    }

    #[test]
    fn an_unprojected_write_is_pending() {
        let r = RolloutInput {
            projected: false,
            ..written(9)
        };
        let out = compute(&r, &[instance("a", true, 100)]);
        assert_eq!(summary(&out), (RolloutState::Pending, 0, 1));
        assert_eq!(out.kine_revision, Some(9));
    }

    #[test]
    fn no_online_instance_means_no_gateways() {
        let out = compute(&written(9), &[]);
        assert_eq!(summary(&out), (RolloutState::NoGateways, 0, 0));
        let out = compute(&written(9), &[instance("a", false, 100)]);
        assert_eq!(summary(&out), (RolloutState::NoGateways, 0, 0));
    }

    #[test]
    fn an_instance_behind_the_write_keeps_it_propagating() {
        let out = compute(
            &written(9),
            &[instance("a", true, 9), instance("b", true, 8)],
        );
        assert_eq!(summary(&out), (RolloutState::Propagating, 1, 2));
    }

    #[test]
    fn every_online_instance_at_or_past_the_write_makes_it_applied() {
        let out = compute(
            &written(9),
            &[instance("a", true, 9), instance("b", true, 40)],
        );
        assert_eq!(summary(&out), (RolloutState::Applied, 2, 2));
        assert_eq!(out.kine_revision, Some(9));
        assert!(out.rejections.is_empty());
    }

    #[test]
    fn an_instance_that_saw_the_write_and_rejected_it_makes_it_rejected() {
        let r = written(9);
        let out = compute(
            &r,
            &[
                rejecting(instance("a", true, 9), &r, "bad-7e1"),
                instance("b", true, 3),
                instance("c", true, 12),
            ],
        );
        assert_eq!(summary(&out), (RolloutState::Rejected, 1, 3));
        assert_eq!(
            out.rejections,
            [json!({"instance_id": "a", "hostname": "host-a", "error": "bad-7e1"})]
        );
        // A rejection of another document is not this one's.
        let other = RolloutInput {
            id: Uuid::from_u128(0x7e2),
            ..written(9)
        };
        let out = compute(&r, &[rejecting(instance("a", true, 9), &other, "x")]);
        assert_eq!(summary(&out), (RolloutState::Applied, 1, 1));
        let other_kind = RolloutInput {
            kind: Kind::ProviderKeys,
            ..written(9)
        };
        let out = compute(&r, &[rejecting(instance("a", true, 9), &other_kind, "x")]);
        assert_eq!(summary(&out), (RolloutState::Applied, 1, 1));
    }

    #[test]
    fn a_rejection_from_an_instance_that_has_not_seen_the_latest_write_is_not_reported() {
        let r = written(9);
        let out = compute(&r, &[rejecting(instance("a", true, 8), &r, "old-7e1")]);
        assert_eq!(summary(&out), (RolloutState::Propagating, 0, 1));
        assert!(out.rejections.is_empty(), "{out:?}");
    }

    #[test]
    fn offline_instances_never_hold_a_rollout_back() {
        let r = written(9);
        let out = compute(
            &r,
            &[
                instance("a", true, 9),
                instance("b", false, 1),
                rejecting(instance("c", false, 9), &r, "x"),
            ],
        );
        assert_eq!(summary(&out), (RolloutState::Applied, 1, 1));
        assert!(out.rejections.is_empty());
    }

    #[test]
    fn a_runtime_guardrail_rejection_maps_to_its_guardrail() {
        let env = EnvSettings {
            env_id: Uuid::from_u128(0xe7),
            etcd_prefix: "/tenant-root".to_string(),
        };
        let id = "6d5f1c34-0f6b-4e43-9d8f-1f1b0f6b7e11";
        // The gateway names a guardrail whose runtime build failed under a
        // fixed `/aisix` root, whatever the deployment's prefix.
        assert_eq!(
            resource_of_rejected_key(&env, &format!("/aisix/runtime/guardrails/{id}")),
            Some((Kind::Guardrails, id.to_string()))
        );
        assert_eq!(
            resource_of_rejected_key(&env, &format!("{}models/{id}", env.env_prefix())),
            Some((Kind::Models, id.to_string()))
        );
        assert_eq!(
            resource_of_rejected_key(
                &env,
                &format!("{}budget_policy/{}", env.env_prefix(), env.env_id)
            ),
            Some((Kind::BudgetPolicy, env.env_id.to_string()))
        );
        let other_env = EnvSettings {
            env_id: Uuid::from_u128(0xe8),
            ..env.clone()
        };
        for key in [
            format!("{}models/{id}", other_env.env_prefix()),
            format!("{}models/{id}", env.global_prefix()),
            format!("{}not_a_kind/{id}", env.env_prefix()),
            format!("{}models/{id}/extra", env.env_prefix()),
            format!("{}models/", env.env_prefix()),
            "/aisix/runtime/guardrails/".to_string(),
            format!("/aisix/runtime/models/{id}"),
        ] {
            assert_eq!(resource_of_rejected_key(&env, &key), None, "{key}");
        }
    }

    #[test]
    fn a_deleted_resource_is_applied_once_instances_pass_the_delete_revision() {
        // A delete kine recorded at revision 9 (the row is a tombstone).
        let delete = written(9);
        let out = compute(&delete, &[instance("a", true, 8)]);
        assert_eq!(summary(&out), (RolloutState::Propagating, 0, 1));
        let out = compute(&delete, &[instance("a", true, 9)]);
        assert_eq!(summary(&out), (RolloutState::Applied, 1, 1));
    }

    #[test]
    fn a_delete_kine_held_no_key_for_waits_for_no_revision() {
        // Projected while kine held no key, so kine has no revision for it:
        // no gateway can be behind on what kine never recorded, and a
        // gateway whose revision advances only through its own key spaces
        // would never reach any other.
        let gone = RolloutInput {
            kine_revision: None,
            ..written(9)
        };
        let out = compute(&gone, &[instance("a", true, 0), instance("b", true, 3)]);
        assert_eq!(summary(&out), (RolloutState::Applied, 2, 2));
        assert_eq!(out.kine_revision, None);
        let out = compute(&gone, &[instance("a", false, 3)]);
        assert_eq!(summary(&out), (RolloutState::NoGateways, 0, 0));
    }

    #[test]
    fn while_the_fleet_view_is_incomplete_nothing_reads_as_applied() {
        let r = written(9);
        let fleet = |incomplete: bool, instances: Vec<InstanceView>| Fleet {
            instances,
            cap_hit_at: incomplete.then(chrono::Utc::now),
        };
        let complete = fleet(false, vec![instance("a", true, 9)]);
        assert_eq!(complete.rollout(&r).state, RolloutState::Applied);
        let capped = fleet(true, vec![instance("a", true, 9)]);
        let out = capped.rollout(&r);
        assert_eq!(summary(&out), (RolloutState::Propagating, 1, 1));
        let gone = RolloutInput {
            kine_revision: None,
            ..written(9)
        };
        assert_eq!(capped.rollout(&gone).state, RolloutState::Propagating);
        // The other states say nothing about the gateways the view misses.
        let rejected = fleet(true, vec![rejecting(instance("a", true, 9), &r, "x")]);
        assert_eq!(rejected.rollout(&r).state, RolloutState::Rejected);
        let unprojected = RolloutInput {
            projected: false,
            ..written(9)
        };
        assert_eq!(capped.rollout(&unprojected).state, RolloutState::Pending);
    }

    #[test]
    fn the_states_are_named_as_the_api_names_them() {
        let names: Vec<Value> = [
            RolloutState::Pending,
            RolloutState::Propagating,
            RolloutState::Applied,
            RolloutState::Rejected,
            RolloutState::NoGateways,
        ]
        .iter()
        .map(|s| serde_json::to_value(s).expect("serialize"))
        .collect();
        assert_eq!(
            names,
            [
                json!("pending"),
                json!("propagating"),
                json!("applied"),
                json!("rejected"),
                json!("no_gateways")
            ]
        );
    }
}
