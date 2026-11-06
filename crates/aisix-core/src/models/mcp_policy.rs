//! `McpPolicy` entity — environment-level and team-level MCP tool access
//! policies stored in etcd under `mcp_policies/<uuid>`.
//!
//! MCP tool access is resolved from up to three layers of the same shape,
//! each an `allow`/`deny` pattern pair: the `env`-scoped policy, the
//! `team`-scoped policy of the key's team, and the key's own `mcp_access`
//! block. A tool is permitted when **every present layer** allows it and
//! **no** layer denies it — allow intersects, deny unions. A layer that is
//! absent (no policy row, a disabled one, or no `mcp_access` block) imposes
//! no constraint; with no layer present at all the grant is empty, so MCP
//! access is always granted explicitly.
//!
//! Because `allow` is required on every layer, a layer that only wants to
//! subtract tools spells its allow side `["*"]`. The effective-ACL
//! computation lives with the MCP gateway endpoint, which resolves it per
//! request.

use serde::{Deserialize, Serialize};

use super::mcp_ref::McpToolRef;
use crate::resource::Resource;

/// Which API keys an MCP access policy applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpPolicyScope {
    /// Applied to every key in the environment.
    Env,
    /// Applied to the keys belonging to the team named by `scope_ref`, on
    /// top of the environment policy rather than in place of it.
    Team,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpPolicy {
    /// Which API keys the policy applies to: the whole environment or one
    /// team.
    pub scope: McpPolicyScope,

    /// Team identifier the policy targets. Required when `scope` is `team`;
    /// omitted for an environment policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub scope_ref: Option<String>,

    /// Namespaced `<server>__<tool>` patterns this layer allows. Entries are
    /// matched as `*` globs: `"*"` allows every tool, `"<server>__*"`
    /// every tool on one server, and an entry without a `*` matches one tool
    /// exactly. An empty list allows nothing, which is how a policy blocks
    /// all MCP access; a policy that only means to subtract tools writes
    /// `["*"]` here and lists them under `deny`.
    ///
    /// The write path requires the field (the strict schema adds it to
    /// `required`), so a layer never allows something by omission. The
    /// runtime loader defaults it to empty instead of rejecting the row:
    /// a document written before the layered shape would otherwise fail
    /// to deserialize, and a skipped `api_key` row stops authenticating
    /// altogether rather than merely losing MCP access.
    ///
    /// Read only when `allow_ids` is absent; ignored entirely when it is
    /// present.
    #[serde(default)]
    pub allow: Vec<String>,

    /// This layer's allow side written by MCP server resource id instead of
    /// by server name, so renaming a server does not change what the layer
    /// allows. Each entry names one MCP server by the id it is registered
    /// under and one tool by name, the tool matched as a `*` glob
    /// against the bare tool name.
    ///
    /// Present — including as an empty array — it is authoritative and
    /// `allow` is ignored; an empty array therefore allows nothing. Set to
    /// `null` it means the same as omitted: the layer falls back to `allow`.
    ///
    /// It cannot express "every server": each entry names one server
    /// exactly and `server_id` is never a glob, so an enumeration of the
    /// servers registered today silently fails to cover one registered
    /// tomorrow. To allow (or deny) every server, present and future,
    /// leave this absent and use the name form's `"*"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_ids: Option<Vec<McpToolRef>>,

    /// Namespaced `<server>__<tool>` patterns subtracted from the effective
    /// grant of every key the policy applies to, using the same `*`
    /// glob matching as `allow`. Deny always wins: a tool matched here stays
    /// unavailable however the other layers allow it.
    ///
    /// Read only when `deny_ids` is absent; ignored entirely when it is
    /// present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,

    /// This layer's deny side written by MCP server resource id instead of
    /// by server name. Present — including as an empty array — it is
    /// authoritative and `deny` is ignored; an empty array subtracts
    /// nothing. Absent or `null`, the layer falls back to `deny`.
    ///
    /// It cannot express "every server": each entry names one server
    /// exactly and `server_id` is never a glob, so an enumeration of the
    /// servers registered today silently fails to cover one registered
    /// tomorrow. To allow (or deny) every server, present and future,
    /// leave this absent and use the name form's `"*"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_ids: Option<Vec<McpToolRef>>,

    /// Whether the policy is applied. A disabled policy is kept but
    /// contributes neither its allow nor its deny side. Treated as `true`
    /// when omitted.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// etcd-key uuid. Filled by the loader and never included in the JSON
    /// payload.
    #[serde(skip)]
    pub(crate) runtime_id: String,
}

fn default_enabled() -> bool {
    true
}

/// The API key's own layer of the MCP tool ACL, the same `allow`/`deny`
/// shape an MCP access policy carries. Present means the key constrains its
/// grant; omitted means the key adds no constraint of its own and takes
/// whatever the environment and team layers leave.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpAccess {
    /// Namespaced `<server>__<tool>` patterns this key allows, intersected
    /// with the environment and team layers. Same `*` glob matching a
    /// policy's `allow` uses; an empty list leaves the key no MCP access,
    /// and `["*"]` narrows nothing (useful with `deny` alone).
    ///
    /// Required on the write path and defaulted by the runtime loader,
    /// for the reason given on [`McpPolicy::allow`]. Read only when
    /// `allow_ids` is absent; ignored entirely when it is present.
    #[serde(default)]
    pub allow: Vec<String>,

    /// This key's allow side written by MCP server resource id instead of by
    /// server name, so renaming a server does not change what the key may
    /// reach. Present — including as an empty array — it is authoritative
    /// and `allow` is ignored; an empty array therefore allows nothing.
    /// Absent or `null`, the key falls back to `allow`.
    ///
    /// It cannot express "every server": each entry names one server
    /// exactly and `server_id` is never a glob, so an enumeration of the
    /// servers registered today silently fails to cover one registered
    /// tomorrow. To allow (or deny) every server, present and future,
    /// leave this absent and use the name form's `"*"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_ids: Option<Vec<McpToolRef>>,

    /// Namespaced `<server>__<tool>` patterns subtracted from this key's
    /// effective grant, using the same `*` glob matching as `allow`.
    /// Read only when `deny_ids` is absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,

    /// This key's deny side written by MCP server resource id instead of by
    /// server name. Present — including as an empty array — it is
    /// authoritative and `deny` is ignored; an empty array subtracts
    /// nothing. Absent or `null`, the key falls back to `deny`.
    ///
    /// It cannot express "every server": each entry names one server
    /// exactly and `server_id` is never a glob, so an enumeration of the
    /// servers registered today silently fails to cover one registered
    /// tomorrow. To allow (or deny) every server, present and future,
    /// leave this absent and use the name form's `"*"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_ids: Option<Vec<McpToolRef>>,
}

/// Reported when a stored key's `mcp_access: {mode: inherit}` loads as no
/// block of its own.
pub const LEGACY_MCP_ACCESS_INHERIT: &str = "legacy:mcp_access.mode_inherit_as_no_key_layer";
/// Reported when a stored key's `allowed_tools` loads as its `mcp_access`
/// allow list.
pub const LEGACY_ALLOWED_TOOLS_AS_ALLOW: &str = "legacy:allowed_tools_as_mcp_access_allow";

/// Bring a stored API key's MCP access forward from the shape a release
/// before the layered ACL wrote, where it means the same thing now, and
/// return a report label per change.
///
/// That release read a key with no `mcp_access` block by its `allowed_tools`
/// alone, and read a block by its `mode`. Two of its shapes need help to keep
/// that meaning; every other one already loads as what it meant, or narrower,
/// with `mode` left as an ordinary unknown field:
///
/// - `{mode: inherit}` and nothing else took the team and environment grant
///   unchanged, which is what no block of its own does now. Read as written,
///   it is a layer allowing nothing.
/// - `allowed_tools` beside no block was the key's whole grant. Read as
///   written it is ignored and the key takes the policy layers instead, which
///   can grant more than the key ever had; as the key's own allow list, it
///   grants at most what it did.
///
/// What is left as it loads: `restrict` intersected its `allow` with the
/// inherited grant, which a key layer does; `deny`, or `restrict` with no
/// `allow`, granted nothing, which an empty allow list does; and `inherit`
/// beside an `allow` or `deny` list loads as that list, which can grant less
/// than it did but never more, since no shape here can subtract a list
/// without also granting everything when no policy is present. `allowed_tools`
/// beside a block was not read, and still is not.
///
/// For the read path only: the strict schema refuses both spellings.
pub fn upgrade_stored_mcp_access(key: &mut serde_json::Value) -> Vec<&'static str> {
    let Some(key) = key.as_object_mut() else {
        return Vec::new();
    };
    let bare_inherit = key
        .get("mcp_access")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|block| {
            block.len() == 1
                && block.get("mode").and_then(serde_json::Value::as_str) == Some("inherit")
        });
    if bare_inherit {
        key.remove("mcp_access");
        // The block superseded `allowed_tools`, so it never applied.
        key.remove("allowed_tools");
        return vec![LEGACY_MCP_ACCESS_INHERIT];
    }
    let has_block = key.get("mcp_access").is_some_and(|block| !block.is_null());
    if has_block
        || !key
            .get("allowed_tools")
            .is_some_and(serde_json::Value::is_array)
    {
        return Vec::new();
    }
    let Some(tools) = key.remove("allowed_tools") else {
        return Vec::new();
    };
    key.insert(
        "mcp_access".to_string(),
        serde_json::json!({ "allow": tools }),
    );
    vec![LEGACY_ALLOWED_TOOLS_AS_ALLOW]
}

impl Resource for McpPolicy {
    fn id(&self) -> &str {
        &self.runtime_id
    }

    /// The by-name index key: the targeted team id, or `"env"` for the
    /// environment policy. Lookups during effective-ACL resolution iterate
    /// and filter on `(scope, scope_ref)` rather than relying on this index,
    /// so a malformed row can never shadow the environment layer.
    #[allow(clippy::misnamed_getters)]
    fn name(&self) -> &str {
        self.scope_ref.as_deref().unwrap_or("env")
    }

    fn kind() -> &'static str {
        "mcp_policies"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each MCP access shape a key could be stored with before the layered
    /// ACL, read as it is now. `None` is no block of its own.
    #[test]
    fn a_stored_key_keeps_what_its_mcp_access_meant() {
        let read = |stored: serde_json::Value| {
            let mut key = serde_json::json!({ "key_hash": "h", "allowed_models": [] });
            key.as_object_mut()
                .unwrap()
                .extend(stored.as_object().unwrap().clone());
            let labels = upgrade_stored_mcp_access(&mut key);
            let access = key
                .get("mcp_access")
                .map(|block| serde_json::from_value::<McpAccess>(block.clone()).unwrap())
                .map(|a| (a.allow, a.deny));
            (access, labels, key.get("allowed_tools").is_some())
        };
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // Took the team and environment grant unchanged.
        assert_eq!(
            read(serde_json::json!({ "mcp_access": { "mode": "inherit" } })),
            (None, vec![LEGACY_MCP_ACCESS_INHERIT], false)
        );
        assert_eq!(
            read(serde_json::json!({
                "mcp_access": { "mode": "inherit" }, "allowed_tools": ["x__*"]
            })),
            (None, vec![LEGACY_MCP_ACCESS_INHERIT], false),
            "the block superseded allowed_tools"
        );
        // Was the key's whole grant.
        assert_eq!(
            read(serde_json::json!({ "allowed_tools": ["github__*"] })),
            (
                Some((s(&["github__*"]), vec![])),
                vec![LEGACY_ALLOWED_TOOLS_AS_ALLOW],
                false
            )
        );
        assert_eq!(
            read(serde_json::json!({ "allowed_tools": [] })),
            (
                Some((vec![], vec![])),
                vec![LEGACY_ALLOWED_TOOLS_AS_ALLOW],
                false
            ),
            "an empty list granted nothing, and still does"
        );

        // Already load as what they meant, with `mode` an unknown field.
        assert_eq!(
            read(serde_json::json!({
                "mcp_access": { "mode": "restrict", "allow": ["a__*"], "deny": ["a__b"] }
            })),
            (Some((s(&["a__*"]), s(&["a__b"]))), vec![], false)
        );
        for stored in [
            serde_json::json!({ "mcp_access": { "mode": "deny" } }),
            serde_json::json!({ "mcp_access": { "mode": "restrict" } }),
        ] {
            assert_eq!(read(stored), (Some((vec![], vec![])), vec![], false));
        }
        // Fails closed: inherited minus a list has no shape that does not
        // also grant everything where no policy is present.
        assert_eq!(
            read(serde_json::json!({ "mcp_access": { "mode": "inherit", "deny": ["a__b"] } })),
            (Some((vec![], s(&["a__b"]))), vec![], false)
        );
        // Not read beside a block then, not read now.
        assert_eq!(
            read(serde_json::json!({
                "mcp_access": { "allow": ["a__*"] }, "allowed_tools": ["b__*"]
            })),
            (Some((s(&["a__*"]), vec![])), vec![], true)
        );
        // Both readings exist of this one: the layered half of a hybrid a
        // control plane wrote for older gateways, and a key that had no
        // access. It loads as the layered half.
        assert_eq!(
            read(serde_json::json!({ "mcp_access": { "mode": "deny", "allow": ["a__*"] } })),
            (Some((s(&["a__*"]), vec![])), vec![], false)
        );
        // The current shape is left as written.
        assert_eq!(
            read(serde_json::json!({ "mcp_access": { "allow": ["a__*"] } })),
            (Some((s(&["a__*"]), vec![])), vec![], false)
        );
        assert_eq!(read(serde_json::json!({})), (None, vec![], false));
    }

    /// An MCP policy's retired `mode`, read as it is now. `selected` and
    /// `none` load as what they meant. `all` cannot: on that release a key
    /// with no block of its own never took a policy's grant, and now every
    /// such key does, so granting everything here would hand every tool to
    /// keys that had none. It fails closed and is reported as an unknown
    /// field.
    #[test]
    fn a_stored_policy_mode_loads_as_what_it_meant_or_fails_closed() {
        let allow =
            |stored: serde_json::Value| serde_json::from_value::<McpPolicy>(stored).unwrap().allow;
        assert_eq!(
            allow(serde_json::json!({ "scope": "env", "mode": "selected", "allow": ["a__*"] })),
            ["a__*"]
        );
        assert!(allow(serde_json::json!({ "scope": "env", "mode": "none" })).is_empty());
        assert!(allow(serde_json::json!({ "scope": "env", "mode": "all" })).is_empty());
    }

    #[test]
    fn deserialises_env_policy() {
        let p: McpPolicy = serde_json::from_str(
            r#"{
              "scope": "env",
              "allow": ["github__*", "postgres__query"],
              "deny": ["github__delete_repository"]
            }"#,
        )
        .unwrap();
        assert_eq!(p.scope, McpPolicyScope::Env);
        assert!(p.scope_ref.is_none());
        assert_eq!(p.allow, vec!["github__*", "postgres__query"]);
        assert_eq!(p.deny, vec!["github__delete_repository"]);
        assert!(p.enabled);
    }

    #[test]
    fn deserialises_team_policy() {
        let p: McpPolicy = serde_json::from_str(
            r#"{"scope": "team", "scope_ref": "team-uuid-1", "allow": ["*"]}"#,
        )
        .unwrap();
        assert_eq!(p.scope, McpPolicyScope::Team);
        assert_eq!(p.scope_ref.as_deref(), Some("team-uuid-1"));
        assert_eq!(p.allow, vec!["*"]);
        assert!(p.deny.is_empty());
    }

    #[test]
    fn the_loader_defaults_a_missing_allow_to_empty() {
        // A row written before the layered shape — e.g. the old
        // `{"mode":"inherit"}` key block — must still deserialize. It
        // resolves to a layer allowing nothing (fail-closed) rather than
        // being skipped, which for an api_key would drop the whole key.
        // The write path still rejects it; see
        // `mcp_policy_requires_an_explicit_allow_side` in schema.rs.
        let p: McpPolicy = serde_json::from_str(r#"{"scope":"env"}"#).unwrap();
        assert!(p.allow.is_empty());

        let a: McpAccess = serde_json::from_str(r#"{"mode":"inherit"}"#).unwrap();
        assert!(a.allow.is_empty());
    }

    #[test]
    fn tolerates_unknown_fields_for_forward_compat() {
        // The control plane may ship new fields ahead of the DP rolling out; serde must
        // accept them. The write path still rejects them via the strict
        // schema validators (validate_mcp_policy in models/schema.rs).
        let p: McpPolicy =
            serde_json::from_str(r#"{"scope":"env","allow":["*"],"extra":1}"#).unwrap();
        assert_eq!(p.allow, vec!["*"]);
    }

    #[test]
    fn rejects_unknown_scope() {
        assert!(serde_json::from_str::<McpPolicy>(r#"{"scope":"org","allow":["*"]}"#).is_err());
    }

    #[test]
    fn enabled_defaults_true_and_roundtrips_false() {
        let active: McpPolicy = serde_json::from_str(r#"{"scope":"env","allow":["*"]}"#).unwrap();
        assert!(active.enabled);

        let disabled: McpPolicy =
            serde_json::from_str(r#"{"scope":"env","allow":["*"],"enabled":false}"#).unwrap();
        assert!(!disabled.enabled);
    }

    #[test]
    fn resource_trait_points_at_scope_ref_and_kind() {
        assert_eq!(McpPolicy::kind(), "mcp_policies");

        let mut env: McpPolicy = serde_json::from_str(r#"{"scope":"env","allow":["*"]}"#).unwrap();
        env.runtime_id = "p-env".into();
        assert_eq!(env.id(), "p-env");
        assert_eq!(env.name(), "env");

        let mut team: McpPolicy =
            serde_json::from_str(r#"{"scope":"team","scope_ref":"team-uuid-1","allow":[]}"#)
                .unwrap();
        team.runtime_id = "p-team".into();
        assert_eq!(team.name(), "team-uuid-1");
    }

    #[test]
    fn mcp_access_deserialises_allow_and_deny() {
        let narrow: McpAccess =
            serde_json::from_str(r#"{"allow":["github__*"],"deny":["github__delete_repository"]}"#)
                .unwrap();
        assert_eq!(narrow.allow, vec!["github__*"]);
        assert_eq!(narrow.deny, vec!["github__delete_repository"]);

        // The old `mode: deny` is now an empty allow list.
        let blocked: McpAccess = serde_json::from_str(r#"{"allow":[]}"#).unwrap();
        assert!(blocked.allow.is_empty());
        assert!(blocked.deny.is_empty());
    }

    #[test]
    fn empty_deny_stays_off_the_wire() {
        let p: McpPolicy = serde_json::from_str(r#"{"scope":"env","allow":["*"]}"#).unwrap();
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("deny").is_none());
        assert!(v.get("scope_ref").is_none());
        assert_eq!(v["allow"], serde_json::json!(["*"]));

        let a: McpAccess = serde_json::from_str(r#"{"allow":["*"]}"#).unwrap();
        let v = serde_json::to_value(&a).unwrap();
        assert!(v.get("deny").is_none());
    }
}
