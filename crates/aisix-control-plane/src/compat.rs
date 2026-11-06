//! The save-time gateway-version gate (ruling 26): a document is refused
//! while a gateway that would misread it may load it.
//!
//! A gateway loads documents leniently: a field it does not know is
//! ignored, and a row whose value it does not know is skipped. For most
//! fields that is harmless, but for some it opens what the field closes.
//! [`FIELD_FLOORS`] lists those fields with the first gateway release that
//! reads them, and [`gate`] refuses a document that sets one while a
//! registered gateway is older, or while the control plane cannot tell
//! which versions its registered gateways run (ruling P1-R22):
//!
//! - the gateways judged are every instance heard from within
//!   [`FLEET_WINDOW`], online or not, each by the version it last reported:
//!   one offline for a while still serves what it loaded and may come back;
//! - a version that does not read as a release or an unstamped build
//!   refuses ([`GatewayVersion::Unrecognized`]);
//! - while any credential has had to turn a new instance away within the
//!   same window, or has evicted the row of an older gateway the window
//!   still covered (`dp_credentials.instance_cap_hit_at`), the instances
//!   seen are not the whole fleet, and every gated write is refused.
//!
//! The gate judges the gateways registered when the document is saved, so
//! with none registered the write passes; one that registers later with an
//! older version is not stopped by it, and shows in `GET /admin/rollout`
//! (`unsafe`) when it reports ignoring a floored field. A control plane
//! whose own stamp cannot be read does not boot, so the gate never turns
//! itself off. Versions compare by semver precedence (ruling P1-R7):
//! `1.9.0-rc.1` is older than `1.9.0`, and build metadata never counts.

use semver::{BuildMetadata, Version};
use serde_json::Value;
use sqlx::PgConnection;

use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::resources::write::WriteCtx;
use crate::resources::Kind;

/// What makes a document set a floored field. A row keyed on a value
/// ([`Trigger::Equals`], [`Trigger::Contains`]) floors one value of a field
/// older gateways already know, such as an enum value added later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The field is there, with any value but `null`.
    Present,
    /// The field holds this string.
    Equals(&'static str),
    /// The field is a list holding this string.
    Contains(&'static str),
}

/// The first gateway release that reads a floored field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    /// The release this control plane is built for. The release process
    /// replaces it with that release's version once it is tagged.
    ThisRelease,
    Version(&'static str),
}

/// A field older gateways would misread: a document of `kind` whose value
/// at `pointer` (a JSON pointer) meets `trigger` needs gateways at `since`
/// or later, for the reason `why`. `regrown_by` names the lists (JSON
/// pointers) whose growth widens what an older gateway serves through the
/// field it ignores: a replace that keeps the field is judged again when
/// one of them grows.
#[derive(Debug, Clone, Copy)]
pub struct FieldFloor {
    pub kind: Kind,
    pub pointer: &'static str,
    pub trigger: Trigger,
    pub since: Since,
    pub regrown_by: &'static [&'static str],
    pub why: &'static str,
}

impl FieldFloor {
    /// The field as a gateway names it in `partially_compatible_resources`:
    /// the pointer's segments joined by dots (`/a/b` → `a.b`).
    pub fn gateway_field(&self) -> String {
        self.pointer
            .strip_prefix('/')
            .unwrap_or(self.pointer)
            .replace('/', ".")
    }
}

/// The model grants of an api key (`crates/aisix-core/src/models/apikey.rs`).
const KEY_GRANTS: &[&str] = &["/allowed_model_ids", "/allowed_models"];

/// Every field older gateways would misread in a way that opens what it
/// closes, or that quietly withholds what it grants. A new field whose
/// absence is fail-open on an older gateway gets a row here, and so does a
/// new value of a field older gateways know, keyed on that value: an
/// endpoint family added to `allowed_endpoints`, say, which an older
/// gateway reads as its unknown family and grants nothing for.
///
/// `provider_keys` `/groups` has no row: an older gateway ignoring it lets
/// every key use the provider key, as it lets a key that names no groups
/// use every provider key, and the groups restrict only a key that sets
/// `allowed_provider_groups`, which its own row already refuses while such
/// a gateway is registered.
pub const FIELD_FLOORS: &[FieldFloor] = &[
    FieldFloor {
        kind: Kind::CachePolicies,
        pointer: "/scope",
        trigger: Trigger::Equals("team"),
        since: Since::ThisRelease,
        regrown_by: &[],
        why: "an older gateway skips a team-scoped policy and keeps serving the env-scoped \
              one it replaced, or an overlapping env catch-all, which shares cache entries \
              across tenants",
    },
    FieldFloor {
        kind: Kind::ApiKeys,
        pointer: "/allowed_endpoints",
        trigger: Trigger::Present,
        since: Since::ThisRelease,
        regrown_by: KEY_GRANTS,
        why: "an older gateway ignores the field and lets the key call every endpoint",
    },
    FieldFloor {
        kind: Kind::ApiKeys,
        pointer: "/allowed_provider_groups",
        trigger: Trigger::Present,
        since: Since::ThisRelease,
        regrown_by: KEY_GRANTS,
        why: "an older gateway ignores the field and lets the key reach every provider key",
    },
];

/// How recently a gateway instance must have reported to count among the
/// gateways a document is judged by, as a Postgres interval.
pub const FLEET_WINDOW: &str = "interval '24 hours'";

/// A version a gateway reports in its heartbeat (`aisix_core::HEARTBEAT_VERSION`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayVersion {
    /// A stamped release, `X.Y.Z[-pre]+sha-…`, without its build metadata.
    Release(Version),
    /// An unstamped build, `dev` or `dev+sha-…`, which counts as current.
    Dev,
    /// Anything else, an empty version included: it satisfies no floor.
    Unrecognized,
}

impl GatewayVersion {
    pub fn parse(raw: &str) -> GatewayVersion {
        if raw == "dev" || raw.starts_with("dev+") {
            return GatewayVersion::Dev;
        }
        match Version::parse(raw) {
            Ok(mut version) => {
                version.build = BuildMetadata::EMPTY;
                GatewayVersion::Release(version)
            }
            Err(_) => GatewayVersion::Unrecognized,
        }
    }
}

/// The lowest gateway version `since` admits, judged by a control plane
/// reporting `cp_version`: [`Since::ThisRelease`] is that release (a
/// pre-release included, build metadata not), and `None` when the control
/// plane is an unstamped build, which cannot tell which release it is.
pub fn minimum(since: &Since, cp_version: &str) -> Option<Version> {
    match since {
        Since::Version(v) => Version::parse(v).ok(),
        Since::ThisRelease => match GatewayVersion::parse(cp_version) {
            GatewayVersion::Release(v) => Some(v),
            GatewayVersion::Dev | GatewayVersion::Unrecognized => None,
        },
    }
}

/// A floor some gateways do not meet: `below` release gateways are older
/// than `minimum`, and `unrecognized` report a version that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorViolation {
    pub pointer: &'static str,
    pub minimum: String,
    pub below: usize,
    pub unrecognized: usize,
}

impl FloorViolation {
    fn message(&self) -> String {
        let FloorViolation {
            minimum,
            below,
            unrecognized,
            ..
        } = self;
        match (below, unrecognized) {
            (_, 0) => format!("需要网关版本 ≥ {minimum}，当前有 {below} 个实例低于此版本"),
            (0, _) => {
                format!("需要网关版本 ≥ {minimum}，当前有 {unrecognized} 个实例的版本无法识别")
            }
            _ => format!(
                "需要网关版本 ≥ {minimum}，当前有 {below} 个实例低于此版本，\
                 {unrecognized} 个实例的版本无法识别"
            ),
        }
    }
}

fn triggered(floor: &FieldFloor, doc: &Value) -> bool {
    match (doc.pointer(floor.pointer), floor.trigger) {
        (Some(value), Trigger::Present) => !value.is_null(),
        (Some(value), Trigger::Equals(wanted)) => value.as_str() == Some(wanted),
        (Some(value), Trigger::Contains(wanted)) => value
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(wanted))),
        (None, _) => false,
    }
}

/// Whether the list at one of a floor's `regrown_by` pointers grants more
/// after a replace than before (`null` reads as absent): an item added to a
/// list, or the list appearing or going away. Neither absence is narrower
/// than every list on an api key: without `allowed_model_ids` the key
/// falls back to `allowed_models`, and without `allowed_models` it grants
/// none, so a list that appears can grant more as well.
fn grew(before: Option<&Value>, after: Option<&Value>) -> bool {
    fn given(v: Option<&Value>) -> Option<&Value> {
        v.filter(|v| !v.is_null())
    }
    match (given(before), given(after)) {
        (None, None) => false,
        (Some(before), Some(after)) => match (before.as_array(), after.as_array()) {
            (Some(before), Some(after)) => after.iter().any(|item| !before.contains(item)),
            _ => before != after,
        },
        _ => true,
    }
}

/// The rows of `floors` for `kind` that `doc` sets and `stored` (the
/// document it replaces, `null` for a create) does not, or that both set
/// while a list the row's `regrown_by` names grew. A field the stored
/// document already sets is what every gateway already loads, so keeping it
/// exposes nothing new, unless the grants it guards grow; refusing would
/// otherwise stop an operator from changing, or switching off, a row the
/// older gateway already serves.
fn newly_triggered<'f>(
    floors: &'f [FieldFloor],
    kind: Kind,
    doc: &Value,
    stored: &Value,
) -> Vec<&'f FieldFloor> {
    floors
        .iter()
        .filter(|f| f.kind == kind && triggered(f, doc))
        .filter(|f| {
            !triggered(f, stored)
                || f.regrown_by
                    .iter()
                    .any(|p| grew(stored.pointer(p), doc.pointer(p)))
        })
        .collect()
}

fn judge(
    floor: &FieldFloor,
    minimum: &Version,
    fleet: &[GatewayVersion],
) -> Result<(), FloorViolation> {
    let below = fleet
        .iter()
        .filter(|v| matches!(v, GatewayVersion::Release(v) if v.cmp_precedence(minimum).is_lt()))
        .count();
    let unrecognized = fleet
        .iter()
        .filter(|v| **v == GatewayVersion::Unrecognized)
        .count();
    if below == 0 && unrecognized == 0 {
        return Ok(());
    }
    Err(FloorViolation {
        pointer: floor.pointer,
        minimum: minimum.to_string(),
        below,
        unrecognized,
    })
}

/// Whether every gateway of `fleet` reads every floored field a document
/// `doc` of `kind` sets, judged by a control plane reporting `cp_version`:
/// a set field fails when its minimum is known and a release gateway is
/// older, or a gateway's version cannot be read. An unstamped (`dev`)
/// gateway meets every floor.
pub fn check(
    kind: Kind,
    doc: &Value,
    fleet: &[GatewayVersion],
    cp_version: &str,
) -> Result<(), FloorViolation> {
    check_against(FIELD_FLOORS, kind, doc, fleet, cp_version)
}

/// Whether a gateway reporting `version` meets every row of
/// [`FIELD_FLOORS`], judged by a control plane reporting `cp_version`; a row
/// whose minimum it cannot judge is met.
pub fn meets_every_floor(version: &str, cp_version: &str) -> bool {
    let fleet = [GatewayVersion::parse(version)];
    FIELD_FLOORS.iter().all(|floor| {
        minimum(&floor.since, cp_version).is_none_or(|m| judge(floor, &m, &fleet).is_ok())
    })
}

/// [`check`] by the rows of `floors` instead of [`FIELD_FLOORS`].
pub fn check_against(
    floors: &[FieldFloor],
    kind: Kind,
    doc: &Value,
    fleet: &[GatewayVersion],
    cp_version: &str,
) -> Result<(), FloorViolation> {
    for floor in newly_triggered(floors, kind, doc, &Value::Null) {
        if let Some(minimum) = minimum(&floor.since, cp_version) {
            judge(floor, &minimum, fleet)?;
        }
    }
    Ok(())
}

/// `422 gateway_too_old` at `pointer`.
fn too_old(pointer: &str, message: String) -> ApiError {
    ApiError {
        code: "gateway_too_old",
        message: "部分网关版本过低或无法确认版本，暂不能保存此配置".to_string(),
        ..ApiError::validation(vec![Detail {
            path: pointer.to_string(),
            message,
        }])
    }
}

/// The gate `write_resource` runs on a create or a replace of `doc` (of
/// `kind`, replacing `stored`, `null` for a create): `422 gateway_too_old`
/// at the first floored field `doc` newly sets, while a gateway heard from
/// within [`FLEET_WINDOW`] is older than its minimum or reports a version
/// that cannot be read, or while some credential's view of its instances
/// has been incomplete within that window. With no gateway registered the
/// write passes. A write that sets no floored field, or only fields whose
/// minimum this control plane cannot judge, reads nothing.
pub async fn gate(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    kind: Kind,
    doc: &Value,
    stored: &Value,
) -> Result<(), ApiError> {
    let floors: Vec<(&FieldFloor, Version)> = newly_triggered(FIELD_FLOORS, kind, doc, stored)
        .into_iter()
        .filter_map(|f| minimum(&f.since, &ctx.cp_version).map(|m| (f, m)))
        .collect();
    let Some((first, first_minimum)) = floors.first() else {
        return Ok(());
    };
    let versions: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT version FROM dp_instances WHERE last_seen_at > now() - {FLEET_WINDOW}"
    ))
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let fleet: Vec<GatewayVersion> = versions
        .iter()
        .map(|version| GatewayVersion::parse(version))
        .collect();
    for (floor, minimum) in &floors {
        judge(floor, minimum, &fleet).map_err(|v| too_old(v.pointer, v.message()))?;
    }
    let capped: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM dp_credentials \
         WHERE instance_cap_hit_at > now() - {FLEET_WINDOW})"
    ))
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if capped {
        return Err(too_old(
            first.pointer,
            format!(
                "需要网关版本 ≥ {first_minimum}；24 小时内有网关凭据达到实例上限，\
                 部分网关实例未被记录，无法确认全部网关实例的版本"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn versions(raw: &[&str]) -> Vec<GatewayVersion> {
        raw.iter().map(|v| GatewayVersion::parse(v)).collect()
    }

    #[test]
    fn a_team_scope_needs_every_release_gateway_at_or_above_the_minimum() {
        let doc = json!({"name": "c", "scope": "team"});
        let old = [GatewayVersion::parse("1.8.2+sha-a")];
        let new = [GatewayVersion::parse("1.9.0+sha-b")];
        assert!(check(Kind::CachePolicies, &doc, &old, "1.9.0+sha-c").is_err());
        assert!(check(Kind::CachePolicies, &doc, &new, "1.9.0+sha-c").is_ok());
        let fleet = versions(&["1.8.2+sha-a", "1.9.0+sha-b", "1.7.0", "1.10.0+sha-d"]);
        assert_eq!(
            check(Kind::CachePolicies, &doc, &fleet, "1.9.0+sha-c"),
            Err(FloorViolation {
                pointer: "/scope",
                minimum: "1.9.0".to_string(),
                below: 2,
                unrecognized: 0,
            })
        );
    }

    #[test]
    fn an_env_or_api_key_scope_is_never_gated() {
        let old = versions(&["1.0.0+sha-a"]);
        for doc in [
            json!({"name": "c", "scope": "env"}),
            json!({"name": "c", "scope": "api_key"}),
            json!({"name": "c"}),
        ] {
            assert_eq!(
                check(Kind::CachePolicies, &doc, &old, "1.9.0+sha-c"),
                Ok(()),
                "{doc}"
            );
        }
        // A floor binds the kind it names only.
        let team = json!({"name": "c", "scope": "team"});
        assert_eq!(check(Kind::RateLimitPolicies, &team, &old, "1.9.0"), Ok(()));
    }

    #[test]
    fn a_present_field_is_gated_whatever_its_value() {
        let old = versions(&["1.8.2+sha-a"]);
        for (pointer, value) in [
            ("/allowed_endpoints", json!(["chat"])),
            ("/allowed_endpoints", json!([])),
            ("/allowed_provider_groups", json!(["shared"])),
            ("/allowed_provider_groups", json!([])),
        ] {
            let field = pointer.trim_start_matches('/');
            let doc = json!({ "key_hash": "h", field: value });
            let violation = check(Kind::ApiKeys, &doc, &old, "1.9.0+sha-c")
                .expect_err("an older gateway would ignore it");
            assert_eq!(violation.pointer, pointer, "{doc}");
        }
        for doc in [
            json!({"key_hash": "h"}),
            json!({"key_hash": "h", "allowed_endpoints": null}),
        ] {
            assert_eq!(check(Kind::ApiKeys, &doc, &old, "1.9.0"), Ok(()), "{doc}");
        }
    }

    #[test]
    fn a_dev_gateway_satisfies_every_floor() {
        let doc = json!({"name": "c", "scope": "team"});
        let dev = versions(&["dev", "dev+sha-1234abc"]);
        assert_eq!(
            check(Kind::CachePolicies, &doc, &dev, "1.9.0+sha-c"),
            Ok(())
        );
        assert_eq!(dev, [GatewayVersion::Dev, GatewayVersion::Dev]);
    }

    #[test]
    fn a_dev_control_plane_cannot_judge_this_release_and_lets_it_pass() {
        let doc = json!({"name": "c", "scope": "team"});
        let old = versions(&["1.0.0+sha-a", "garbled"]);
        for cp in ["dev", "dev+sha-1234abc"] {
            assert_eq!(minimum(&Since::ThisRelease, cp), None, "{cp}");
            assert_eq!(check(Kind::CachePolicies, &doc, &old, cp), Ok(()), "{cp}");
        }
        // A literal minimum is judged whatever this control plane is.
        assert_eq!(
            minimum(&Since::Version("1.9.0"), "dev"),
            Some(semver::Version::new(1, 9, 0))
        );
    }

    #[test]
    fn a_pre_release_gateway_is_older_than_its_release() {
        let doc = json!({"name": "c", "scope": "team"});
        let rc = versions(&["1.9.0-rc.1+sha-a"]);
        assert!(check(Kind::CachePolicies, &doc, &rc, "1.9.0+sha-c").is_err());
        // A control plane that is itself a pre-release asks for that one.
        assert_eq!(
            minimum(&Since::ThisRelease, "1.9.0-rc.2+sha-c"),
            Some(semver::Version::parse("1.9.0-rc.2").expect("a version"))
        );
        assert!(check(Kind::CachePolicies, &doc, &rc, "1.9.0-rc.2+sha-c").is_err());
        let rc2 = versions(&["1.9.0-rc.2+sha-b", "1.9.0+sha-d"]);
        assert_eq!(
            check(Kind::CachePolicies, &doc, &rc2, "1.9.0-rc.2+sha-c"),
            Ok(())
        );
        // The build metadata never counts.
        assert_eq!(
            minimum(&Since::ThisRelease, "1.9.0+sha-c"),
            Some(semver::Version::new(1, 9, 0))
        );
    }

    #[test]
    fn a_version_that_cannot_be_read_fails_closed() {
        let doc = json!({"name": "c", "scope": "team"});
        for raw in ["", "garbled", "v1.9.0", "1.9", "devel"] {
            assert_eq!(
                GatewayVersion::parse(raw),
                GatewayVersion::Unrecognized,
                "{raw:?}"
            );
            let fleet = versions(&[raw, "1.9.0+sha-b"]);
            assert_eq!(
                check(Kind::CachePolicies, &doc, &fleet, "1.9.0+sha-c"),
                Err(FloorViolation {
                    pointer: "/scope",
                    minimum: "1.9.0".to_string(),
                    below: 0,
                    unrecognized: 1,
                }),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn a_field_the_stored_document_already_carries_is_not_gated_again() {
        let team = json!({"name": "c", "scope": "team", "ttl_seconds": 60});
        let env = json!({"name": "c", "scope": "env"});
        fn count(kind: Kind, doc: &Value, stored: &Value) -> usize {
            newly_triggered(FIELD_FLOORS, kind, doc, stored).len()
        }
        let floors = |doc: &Value, stored: &Value| count(Kind::CachePolicies, doc, stored);
        assert_eq!(floors(&team, &Value::Null), 1, "a create");
        assert_eq!(floors(&team, &env), 1, "env to team");
        assert_eq!(floors(&team, &json!({"name": "c", "scope": "team"})), 0);
        assert_eq!(floors(&env, &team), 0, "team to env");
        let key = |groups: Value| json!({"key_hash": "h", "allowed_provider_groups": groups});
        let keys = |doc: &Value, stored: &Value| count(Kind::ApiKeys, doc, stored);
        assert_eq!(keys(&key(json!(["a", "b"])), &key(json!(["a"]))), 0);
        assert_eq!(keys(&key(json!(["a"])), &json!({"key_hash": "h"})), 1);
    }

    #[test]
    fn a_row_keyed_on_a_value_refuses_an_older_gateway_only_for_that_value() {
        // An endpoint family a later release adds: an older gateway reads it
        // as its unknown family, so the key would not reach that endpoint
        // there, whatever the document says.
        let floors = [FieldFloor {
            kind: Kind::ApiKeys,
            pointer: "/allowed_endpoints",
            trigger: Trigger::Contains("videos_7f1"),
            since: Since::Version("1.10.0"),
            regrown_by: &[],
            why: "test row",
        }];
        let with = json!({"key_hash": "h", "allowed_endpoints": ["chat", "videos_7f1"]});
        let without = json!({"key_hash": "h", "allowed_endpoints": ["chat"]});
        let old = versions(&["1.10.0+sha-a", "1.9.0+sha-b"]);
        let new = versions(&["1.10.0+sha-a", "1.11.0"]);
        assert_eq!(
            check_against(&floors, Kind::ApiKeys, &with, &old, "1.11.0"),
            Err(FloorViolation {
                pointer: "/allowed_endpoints",
                minimum: "1.10.0".to_string(),
                below: 1,
                unrecognized: 0,
            })
        );
        assert_eq!(
            check_against(&floors, Kind::ApiKeys, &without, &old, "1.11.0"),
            Ok(())
        );
        assert_eq!(
            check_against(&floors, Kind::ApiKeys, &with, &new, "1.11.0"),
            Ok(())
        );
        // A scalar holding the value is not a list holding it.
        let scalar = json!({"key_hash": "h", "allowed_endpoints": "videos_7f1"});
        assert_eq!(
            check_against(&floors, Kind::ApiKeys, &scalar, &old, "1.11.0"),
            Ok(())
        );
        // Adding the value to a stored list is new; keeping it is not.
        assert_eq!(
            newly_triggered(&floors, Kind::ApiKeys, &with, &without).len(),
            1
        );
        assert_eq!(
            newly_triggered(&floors, Kind::ApiKeys, &with, &with).len(),
            0
        );
    }

    #[test]
    fn a_kept_field_is_judged_again_when_the_grants_it_guards_grow() {
        fn again(doc: &Value, stored: &Value) -> usize {
            newly_triggered(FIELD_FLOORS, Kind::ApiKeys, doc, stored).len()
        }
        // A key that kept its provider groups, granted models by `grants`.
        let key = |grants: Value| {
            let mut doc = json!({"key_hash": "h", "allowed_provider_groups": ["shared"]});
            if let (Some(doc), Some(grants)) = (doc.as_object_mut(), grants.as_object()) {
                doc.extend(grants.clone());
            }
            doc
        };
        let ids = |list: Value| key(json!({ "allowed_model_ids": list }));
        let names = |list: Value| key(json!({ "allowed_models": list }));
        assert_eq!(
            again(&ids(json!(["m1", "m2"])), &ids(json!(["m1"]))),
            1,
            "a model added"
        );
        assert_eq!(
            again(&ids(json!(["m1"])), &ids(json!(["m1", "m2"]))),
            0,
            "a model withdrawn"
        );
        assert_eq!(
            again(&ids(json!([])), &ids(json!(["m1"]))),
            0,
            "the last one withdrawn"
        );
        assert_eq!(again(&names(json!(["a", "b"])), &names(json!(["a"]))), 1);
        assert_eq!(again(&names(json!(["a"])), &names(json!(["a", "b"]))), 0);
        // Dropping the id list falls back to the names, which may grant more.
        assert_eq!(
            again(&key(json!({})), &ids(json!(["m1"]))),
            1,
            "present to absent"
        );
        // Without an id list the names decide, so a new id list can grant
        // models they did not; without names the key granted none.
        assert_eq!(
            again(&ids(json!(["m1"])), &key(json!({}))),
            1,
            "absent to present"
        );
        assert_eq!(
            again(&ids(Value::Null), &key(json!({}))),
            0,
            "null is absent"
        );
        // Switching the key off keeps every grant as it was.
        let mut disabled = ids(json!(["m1"]));
        disabled["disabled"] = json!(true);
        assert_eq!(again(&disabled, &ids(json!(["m1"]))), 0);
        // A cache policy has no grants to grow.
        let team = json!({"name": "c", "scope": "team", "allowed_model_ids": ["m2"]});
        let kept = json!({"name": "c", "scope": "team"});
        assert_eq!(
            newly_triggered(FIELD_FLOORS, Kind::CachePolicies, &team, &kept).len(),
            0
        );
    }

    #[test]
    fn a_gateway_meets_every_floor_only_at_or_past_every_minimum() {
        assert!(!meets_every_floor("1.8.2+sha-a", "1.9.0+sha-c"));
        assert!(!meets_every_floor("1.9.0-rc.1+sha-a", "1.9.0+sha-c"));
        assert!(
            !meets_every_floor("", "1.9.0+sha-c"),
            "an unreadable version"
        );
        assert!(meets_every_floor("1.9.0+sha-a", "1.9.0+sha-c"));
        assert!(meets_every_floor("dev+sha-a", "1.9.0+sha-c"));
        assert!(
            meets_every_floor("1.0.0", "dev"),
            "a dev control plane judges nothing"
        );
    }

    #[test]
    fn a_floored_pointer_names_the_field_as_the_gateway_reports_it() {
        let floor = |pointer| FieldFloor {
            kind: Kind::Models,
            pointer,
            trigger: Trigger::Present,
            since: Since::ThisRelease,
            regrown_by: &[],
            why: "test row",
        };
        assert_eq!(
            floor("/allowed_endpoints").gateway_field(),
            "allowed_endpoints"
        );
        assert_eq!(floor("/semantic/match").gateway_field(), "semantic.match");
    }

    #[test]
    fn every_literal_minimum_is_a_version() {
        // Asserts nothing until a row names a literal version: every row is
        // `ThisRelease` until the release that ships it is tagged.
        for floor in FIELD_FLOORS {
            if let Since::Version(v) = floor.since {
                assert!(semver::Version::parse(v).is_ok(), "{}: {v}", floor.pointer);
            }
            assert!(floor.pointer.starts_with('/'), "{}", floor.pointer);
        }
        // The rows ruling 26 sets, and the grants each one guards.
        let rows: Vec<(Kind, &str, &[&str])> = FIELD_FLOORS
            .iter()
            .map(|f| (f.kind, f.pointer, f.regrown_by))
            .collect();
        let grants: &[&str] = &["/allowed_model_ids", "/allowed_models"];
        assert_eq!(
            rows,
            [
                (Kind::CachePolicies, "/scope", &[][..]),
                (Kind::ApiKeys, "/allowed_endpoints", grants),
                (Kind::ApiKeys, "/allowed_provider_groups", grants),
            ]
        );
    }

    #[test]
    fn gateway_versions_are_read_as_the_heartbeat_reports_them() {
        assert_eq!(
            GatewayVersion::parse("1.9.0+sha-abc1234"),
            GatewayVersion::Release(semver::Version::new(1, 9, 0))
        );
        assert_eq!(
            GatewayVersion::parse("1.9.0"),
            GatewayVersion::Release(semver::Version::new(1, 9, 0))
        );
        assert_eq!(
            GatewayVersion::parse("1.9.0-rc.1+sha-abc1234"),
            GatewayVersion::Release(semver::Version::parse("1.9.0-rc.1").expect("a version"))
        );
        assert_eq!(GatewayVersion::parse("dev"), GatewayVersion::Dev);
        assert_eq!(
            GatewayVersion::parse("dev+sha-abc1234"),
            GatewayVersion::Dev
        );
    }
}
