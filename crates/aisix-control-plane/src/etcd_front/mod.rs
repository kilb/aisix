//! The etcd front: gateways read their configuration through it, with the
//! etcd API, on the gateway listener's h2 connections.
//!
//! It serves a policy-checked subset of etcd's API ([`policy::RPCS`]): KV
//! `Range` and `Watch`, inside the gateway's own environment's prefix and
//! the global one, forwarded to kine (the etcd API over Postgres) on this
//! host; every write is refused. kine has no access control of its own, so
//! this is the boundary between environments: a gateway is known by its
//! client certificate, admitted by the same rule as on `/dp`, on every RPC.

pub mod policy;
pub mod service;

/// The etcd API, generated from `proto/` by `build.rs`.
#[allow(clippy::all, clippy::pedantic)]
pub mod proto {
    pub mod mvccpb {
        tonic::include_proto!("mvccpb");
    }
    pub mod authpb {
        tonic::include_proto!("authpb");
    }
    pub mod etcdserverpb {
        tonic::include_proto!("etcdserverpb");
    }
}

pub use policy::{AllowedPrefixes, Denial};
pub use service::{kine_endpoint, Front};

use crate::dp::listener::GrpcRoutes;
use proto::etcdserverpb::kv_server::KvServer;
use proto::etcdserverpb::watch_server::WatchServer;

/// What the gateway listener serves on h2: KV and Watch. Lease, Cluster,
/// Maintenance and Auth are not served, so a call to them is
/// `Unimplemented`. Answers may be of any size: kine merges what is queued
/// for a watch into one message (22 MB has been seen), and the gateway
/// decodes up to `i32::MAX`.
pub fn routes(front: Front) -> GrpcRoutes {
    GrpcRoutes::new(KvServer::new(front.clone()).max_encoding_message_size(usize::MAX))
        .add_service(WatchServer::new(front).max_encoding_message_size(usize::MAX))
}

#[cfg(test)]
mod tests {
    use super::policy::{Rpc, RPCS};

    /// The part of `src` before its test module.
    fn production(src: &str) -> &str {
        src.split_once("\n#[cfg(test)]")
            .map_or(src, |(before, _)| before)
    }

    /// `src` without its comment lines.
    fn without_comment_lines(src: &str) -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every `service` of `proto` and the names of its `rpc`s.
    fn proto_services(proto: &str) -> Vec<(String, Vec<String>)> {
        let mut services: Vec<(String, Vec<String>)> = Vec::new();
        for line in proto.lines().map(str::trim) {
            if line.starts_with("//") {
                continue;
            }
            if let Some(rest) = line.strip_prefix("service ") {
                let name = rest.split_whitespace().next().expect("a service name");
                services.push((name.to_string(), Vec::new()));
            } else if let Some(rest) = line.strip_prefix("rpc ") {
                let name = rest.split('(').next().expect("an rpc name").trim();
                services
                    .last_mut()
                    .expect("an rpc inside a service")
                    .1
                    .push(name.to_string());
            }
        }
        services
    }

    /// `DeleteRange` → `delete_range`, `HashKV` → `hash_kv`: the method
    /// name tonic-build generates for an rpc.
    fn snake(name: &str) -> String {
        let mut out = String::new();
        let mut previous: Option<char> = None;
        for c in name.chars() {
            if c.is_ascii_uppercase()
                && previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
            {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            previous = Some(c);
        }
        out
    }

    /// The services `routes` serves: each `<Name>Server::new(` in it.
    fn served(src: &str) -> Vec<String> {
        let routes = src
            .split_once("pub fn routes(")
            .expect("a routes function")
            .1;
        let routes = routes.split_once("\n}").map_or(routes, |(body, _)| body);
        routes
            .match_indices("Server::new(")
            .map(|(at, _)| {
                routes[..at]
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            })
            .collect()
    }

    /// The `async fn`s of `impl <service> for Front`, by name, with their
    /// bodies.
    fn handlers(src: &str, service: &str) -> Vec<(String, String)> {
        let block = src
            .split(&format!("impl {service} for Front {{"))
            .nth(1)
            .unwrap_or_else(|| panic!("service.rs has no `impl {service} for Front`"));
        let block = block.split_once("\n}").map_or(block, |(b, _)| b);
        block
            .split("    async fn ")
            .skip(1)
            .map(|f| {
                let name = f.split('(').next().expect("a name").to_string();
                (name, f.to_string())
            })
            .collect()
    }

    /// The first statement of a handler's `source` (its name, signature and
    /// body), with every whitespace removed, so formatting cannot change it.
    fn first_statement(source: &str) -> String {
        let body = source
            .split_once("{\n")
            .unwrap_or_else(|| panic!("no body in {source}"))
            .1;
        let statement = body.split_once(';').map_or(body, |(first, _)| first);
        format!("{};", statement.split_whitespace().collect::<String>())
    }

    /// Every RPC of every service the front serves has exactly one row in
    /// [`RPCS`], every row names such an RPC, and each handler does what
    /// its row says: an allowed one admits the gateway first and refuses
    /// nothing outright; a refused one admits the gateway, answers
    /// read-only, and forwards nothing. A method added to the protos, or a
    /// service added to `routes`, fails here until it has a decision.
    #[test]
    fn every_rpc_of_a_served_service_has_a_decision() {
        let proto = proto_services(include_str!("../../proto/rpc.proto"));
        assert!(proto.len() >= 6, "{proto:?}");
        let routes_src = without_comment_lines(production(include_str!("mod.rs")));
        for hidden in ["RoutesBuilder", "into_axum_router", "Routes::from("] {
            assert!(
                !routes_src.contains(hidden),
                "mod.rs uses {hidden}, which this census cannot read"
            );
        }
        let served = served(&routes_src);
        assert_eq!(served.len(), 2, "{served:?}");
        let handlers_src = without_comment_lines(production(include_str!("service.rs")));

        for (row, (service, method, _)) in RPCS.iter().enumerate() {
            assert!(
                !RPCS[..row]
                    .iter()
                    .any(|(s, m, _)| s == service && m == method),
                "{service}.{method} has two rows"
            );
            assert!(
                served.iter().any(|s| s.eq_ignore_ascii_case(service)),
                "{service}.{method} has a row, but routes does not serve {service}"
            );
            assert!(
                proto
                    .iter()
                    .any(|(s, ms)| s == service && ms.iter().any(|m| m == method)),
                "{service}.{method} has a row, but rpc.proto has no such rpc"
            );
        }

        for server in &served {
            let (service, methods) = proto
                .iter()
                .find(|(s, _)| s.eq_ignore_ascii_case(server))
                .unwrap_or_else(|| panic!("routes serves {server}, which rpc.proto lacks"));
            let handlers = handlers(&handlers_src, server);
            assert_eq!(
                handlers.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                methods.iter().map(|m| snake(m)).collect::<Vec<_>>(),
                "the handlers of {service} are its rpcs"
            );
            for method in methods {
                let Some((_, _, decision)) =
                    RPCS.iter().find(|(s, m, _)| s == service && m == method)
                else {
                    panic!("{service}.{method} has no row in policy::RPCS: decide it");
                };
                let (_, body) = handlers
                    .iter()
                    .find(|(n, _)| *n == snake(method))
                    .expect("a handler");
                let first = first_statement(body);
                assert!(
                    ["", "letcaller="].iter().any(|binding| {
                        first == format!("{binding}self.admit(request.extensions()).await?;")
                    }),
                    "{service}.{method} must admit the gateway before anything else; its first \
                     statement is `{first}`"
                );
                match decision {
                    Rpc::Allowed => assert!(
                        !body.contains("Denial::ReadOnly"),
                        "{service}.{method} is allowed, but refuses"
                    ),
                    Rpc::ReadOnly => {
                        assert!(
                            body.contains("Denial::ReadOnly"),
                            "{service}.{method} must answer read-only"
                        );
                        for forward in ["Client::new", "self.shared", "self.endpoint"] {
                            assert!(
                                !body.contains(forward),
                                "{service}.{method} is refused, but forwards ({forward})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_first_statement_is_read_up_to_its_semicolon() {
        let handler = "range(\n        &self,\n        request: Request<RangeRequest>,\n    ) \
                       -> Result<Response<RangeResponse>, Status> {\n        let caller = \
                       self\n            .admit(request.extensions())\n            .await?;\n        \
                       other();\n";
        assert_eq!(
            first_statement(handler),
            "letcaller=self.admit(request.extensions()).await?;"
        );
    }

    #[test]
    fn rpc_names_become_the_generated_method_names() {
        assert_eq!(snake("Range"), "range");
        assert_eq!(snake("DeleteRange"), "delete_range");
        assert_eq!(snake("HashKV"), "hash_kv");
        assert_eq!(snake("LeaseKeepAlive"), "lease_keep_alive");
    }
}
