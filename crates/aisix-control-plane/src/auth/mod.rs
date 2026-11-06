//! Who is asking: passwords, database sessions, and the extractors that
//! turn a session cookie into an identity with a role.

pub mod extract;
pub mod password;
pub mod session;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub use extract::OperatorAuth;

/// An operator's role. Ordered: each role may do everything the ones before
/// it may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperatorRole {
    Viewer,
    Operator,
    Admin,
}

impl OperatorRole {
    /// The name stored in the database and used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            OperatorRole::Viewer => "viewer",
            OperatorRole::Operator => "operator",
            OperatorRole::Admin => "admin",
        }
    }
}

/// A name that is not one of the roles.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown role")]
pub struct UnknownRole;

impl FromStr for OperatorRole {
    type Err = UnknownRole;

    fn from_str(s: &str) -> Result<OperatorRole, UnknownRole> {
        match s {
            "viewer" => Ok(OperatorRole::Viewer),
            "operator" => Ok(OperatorRole::Operator),
            "admin" => Ok(OperatorRole::Admin),
            _ => Err(UnknownRole),
        }
    }
}

impl fmt::Display for OperatorRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A tenant member's role. Tenant routes name the roles they admit in
/// `http::ROUTE_ROLES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Owner,
    Admin,
    Member,
    Billing,
}
