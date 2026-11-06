//! What the etcd front lets a gateway read, and which RPCs it serves.
//!
//! A gateway reads two key spaces: its environment's `<prefix>/<env_id>/`
//! and the shared `<prefix>/global/`, both with their trailing slash, so
//! neither also covers an environment whose id starts with this one's
//! (`EtcdConfig::effective_prefix` in aisix-core). Each range is checked in
//! etcd's own terms, `[key, range_end)`, an empty `range_end` being the one
//! key, and must lie wholly inside one of the two.

use tonic::Status;
use uuid::Uuid;

use crate::settings::EnvSettings;

/// The key spaces a gateway of this environment may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedPrefixes {
    pub env: Vec<u8>,
    pub global: Vec<u8>,
}

impl AllowedPrefixes {
    /// This environment's two prefixes.
    pub fn of(env: &EnvSettings) -> AllowedPrefixes {
        AllowedPrefixes {
            env: env.env_prefix().into_bytes(),
            global: env.global_prefix().into_bytes(),
        }
    }
}

/// Why the front refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// A key or range outside the gateway's key spaces.
    OutsidePrefix,
    /// A write: the front serves reads only.
    ReadOnly,
}

impl Denial {
    /// The answer a gateway gets: `PermissionDenied`, in the words the
    /// gateway's own fixtures use for an etcd-auth refusal
    /// (`crates/aisix-etcd/src/supervisor.rs`). `env_id` is this
    /// environment's.
    pub fn into_status(self, env_id: Uuid) -> Status {
        match self {
            Denial::OutsidePrefix => Status::permission_denied(format!(
                "etcdserver: permission denied: outside env {env_id} prefix"
            )),
            Denial::ReadOnly => {
                Status::permission_denied("etcdserver: permission denied: read-only endpoint")
            }
        }
    }
}

/// The first key past every key that starts with `prefix` (etcd's
/// "prefix + 1"), or `\0` when there is none.
pub fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return end;
        }
    }
    vec![0]
}

/// Whether `[key, range_end)` lies inside one allowed prefix `p`: `key`
/// starts with `p`, and `range_end` is empty (the one key) or ends the
/// range at or before `prefix_end(p)`. `\0` as `range_end` means every key
/// from `key` on, so it is refused whatever the prefix.
pub fn check_range(allowed: &AllowedPrefixes, key: &[u8], range_end: &[u8]) -> Result<(), Denial> {
    let inside = [&allowed.env, &allowed.global].into_iter().any(|p| {
        key.starts_with(p)
            && (range_end.is_empty() || (range_end != [0] && range_end <= prefix_end(p).as_slice()))
    });
    if inside {
        Ok(())
    } else {
        Err(Denial::OutsidePrefix)
    }
}

/// What the front does with one RPC of a service it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rpc {
    /// Served, inside the gateway's key spaces.
    Allowed,
    /// Refused with [`Denial::ReadOnly`].
    ReadOnly,
}

/// Every RPC of the services the front serves (`etcd_front::routes`), as
/// `(service, rpc, decision)` in `proto/rpc.proto`'s names. The others,
/// Lease, Cluster, Maintenance and Auth, are not served at all: a call to
/// them is `Unimplemented`. `every_rpc_of_a_served_service_has_a_decision`
/// fails while an RPC of a served service has no row here.
pub const RPCS: &[(&str, &str, Rpc)] = &[
    ("KV", "Range", Rpc::Allowed),
    ("KV", "Put", Rpc::ReadOnly),
    ("KV", "DeleteRange", Rpc::ReadOnly),
    ("KV", "Txn", Rpc::ReadOnly),
    ("KV", "Compact", Rpc::ReadOnly),
    ("Watch", "Watch", Rpc::Allowed),
];

#[cfg(test)]
mod tests {
    use tonic::Code;
    use uuid::Uuid;

    use super::*;

    fn allowed() -> AllowedPrefixes {
        AllowedPrefixes {
            env: b"/aisix/e1/".to_vec(),
            global: b"/aisix/global/".to_vec(),
        }
    }

    #[test]
    fn a_prefix_range_inside_the_env_is_allowed() {
        assert!(check_range(&allowed(), b"/aisix/e1/", b"/aisix/e10").is_ok());
    }

    #[test]
    fn the_bare_env_prefix_without_its_slash_is_denied() {
        assert_eq!(
            check_range(&allowed(), b"/aisix/e1", b"/aisix/e2"),
            Err(Denial::OutsidePrefix)
        );
        assert_eq!(
            check_range(&allowed(), b"/aisix/e1", b""),
            Err(Denial::OutsidePrefix)
        );
    }

    #[test]
    fn another_environment_is_denied() {
        for (key, end) in [
            (&b"/aisix/e2/"[..], &b"/aisix/e20"[..]),
            // An environment whose id starts with this one's.
            (b"/aisix/e10/", b"/aisix/e100"),
            (b"/aisix/e1x/", b"/aisix/e1y"),
        ] {
            assert_eq!(
                check_range(&allowed(), key, end),
                Err(Denial::OutsidePrefix),
                "{key:?}"
            );
        }
    }

    #[test]
    fn a_range_end_past_the_prefix_is_denied() {
        for end in [
            &b"/aisix/e10\0"[..],
            b"/aisix/e11",
            b"/aisix/global0",
            b"/aisix/z",
            // `\0`: every key from `key` on.
            b"\0",
        ] {
            assert_eq!(
                check_range(&allowed(), b"/aisix/e1/", end),
                Err(Denial::OutsidePrefix),
                "{end:?}"
            );
        }
    }

    #[test]
    fn a_continued_page_inside_the_prefix_is_allowed() {
        assert!(check_range(&allowed(), b"/aisix/e1/models/abc\0", b"/aisix/e10").is_ok());
        // Keys that differ only after the prefix, and a range inside it.
        assert!(check_range(&allowed(), b"/aisix/e1/models/", b"/aisix/e1/models0").is_ok());
        assert!(check_range(&allowed(), b"/aisix/e1/\xff", b"/aisix/e1/\xff\xff").is_ok());
    }

    #[test]
    fn a_single_key_in_the_env_is_allowed_and_one_in_another_env_is_not() {
        assert!(check_range(&allowed(), b"/aisix/e1/models/a", b"").is_ok());
        assert_eq!(
            check_range(&allowed(), b"/aisix/e2/models/a", b""),
            Err(Denial::OutsidePrefix)
        );
    }

    #[test]
    fn the_global_prefix_is_readable() {
        assert!(check_range(&allowed(), b"/aisix/global/", b"/aisix/global0").is_ok());
        assert!(check_range(&allowed(), b"/aisix/global/pricing/p1", b"").is_ok());
        assert_eq!(
            check_range(&allowed(), b"/aisix/global", b"/aisix/globam"),
            Err(Denial::OutsidePrefix)
        );
        assert_eq!(
            check_range(&allowed(), b"/aisix/global/", b"/aisix/h"),
            Err(Denial::OutsidePrefix)
        );
    }

    #[test]
    fn an_empty_key_and_the_all_keys_sentinel_are_denied() {
        for (key, end) in [
            (&b""[..], &b"/aisix/e10"[..]),
            (b"", b""),
            (b"", b"\0"),
            (b"\0", b"\0"),
            (b"/aisix/", b"/aisix0"),
        ] {
            assert_eq!(
                check_range(&allowed(), key, end),
                Err(Denial::OutsidePrefix),
                "{key:?}..{end:?}"
            );
        }
    }

    #[test]
    fn prefix_end_is_the_first_key_past_the_prefix() {
        assert_eq!(prefix_end(b"/aisix/e1/"), b"/aisix/e10");
        assert_eq!(prefix_end(b"a\xff"), b"b");
        assert_eq!(prefix_end(b"\xff\xff"), b"\0");
        // A prefix with no end past it is still not "every key from here":
        // `\0` is refused as a range end whatever the prefix.
        let all_ff = AllowedPrefixes {
            env: b"\xff".to_vec(),
            global: b"/aisix/global/".to_vec(),
        };
        assert_eq!(
            check_range(&all_ff, b"\xff", b"\0"),
            Err(Denial::OutsidePrefix)
        );
    }

    #[test]
    fn the_prefixes_are_the_environments_and_the_global_one() {
        let env = crate::settings::EnvSettings {
            env_id: Uuid::nil(),
            etcd_prefix: "/aisix/".to_string(),
        };
        assert_eq!(
            AllowedPrefixes::of(&env),
            AllowedPrefixes {
                env: b"/aisix/00000000-0000-0000-0000-000000000000/".to_vec(),
                global: b"/aisix/global/".to_vec(),
            }
        );
    }

    #[test]
    fn a_denial_is_permission_denied_in_the_gateways_words() {
        let env_id = Uuid::new_v4();
        let status = Denial::OutsidePrefix.into_status(env_id);
        assert_eq!(status.code(), Code::PermissionDenied);
        assert_eq!(
            status.message(),
            format!("etcdserver: permission denied: outside env {env_id} prefix")
        );
        let status = Denial::ReadOnly.into_status(env_id);
        assert_eq!(status.code(), Code::PermissionDenied);
        assert_eq!(
            status.message(),
            "etcdserver: permission denied: read-only endpoint"
        );
    }
}
