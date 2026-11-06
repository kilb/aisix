//! Who is asking: passwords, database sessions, and the extractors that
//! turn a session cookie into an identity with a role.

pub mod extract;
pub mod password;
pub mod session;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub use extract::{MemberAuth, OperatorAuth};

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
///
/// - `owner`: everything, including making and changing other owners.
/// - `admin`: manages members and every key, but cannot make an owner or
///   change one.
/// - `member`: uses keys, their own.
/// - `billing`: reads only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Owner,
    Admin,
    Member,
    Billing,
}

impl MemberRole {
    /// The name stored in the database and used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            MemberRole::Owner => "owner",
            MemberRole::Admin => "admin",
            MemberRole::Member => "member",
            MemberRole::Billing => "billing",
        }
    }
}

impl FromStr for MemberRole {
    type Err = UnknownRole;

    fn from_str(s: &str) -> Result<MemberRole, UnknownRole> {
        match s {
            "owner" => Ok(MemberRole::Owner),
            "admin" => Ok(MemberRole::Admin),
            "member" => Ok(MemberRole::Member),
            "billing" => Ok(MemberRole::Billing),
            _ => Err(UnknownRole),
        }
    }
}
