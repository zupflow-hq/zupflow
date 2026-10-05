//! The scope a stored value belongs to, and the one storage layout every
//! adapter uses.
//!
//! A [`Scope`] is built only from the trust context and is always the first
//! argument of the storage traits, so the namespace a value lands in is the
//! server's choice, never the caller's. [`Scope::storage_prefix`] gives each
//! scope its own keyspace, disjoint from every other by construction:
//!
//! ```text
//! org/{org uuid}/o/{path}            an organization's own values
//! org/{org uuid}/e/{environment}/…   an organization's values for one environment
//! org/{org uuid}/w/{ws uuid}/{path}  one workspace's values
//! ```
//!
//! The third segment (`o`, `e` or `w`) decides the shape of the rest, so no
//! path in one scope spells a key in another. UUIDs are lowercase hex, so keys
//! are safe on case-insensitive storage too.

use crate::environment::Environment;
use crate::ids::{OrganizationId, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// Organization-wide data.
    Organization(OrganizationId),
    /// An organization's data for one environment (its configuration for
    /// Sandbox, say), so the environment is part of the scope, not of a name.
    Environment(OrganizationId, Environment),
    /// One workspace's data: always carries its organization too. A workspace
    /// belongs to one environment already.
    Workspace(OrganizationId, WorkspaceId),
}

impl Scope {
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match *self {
            Self::Workspace(_, workspace_id) => Some(workspace_id),
            _ => None,
        }
    }

    pub fn environment(&self) -> Option<Environment> {
        match *self {
            Self::Environment(_, environment) => Some(environment),
            _ => None,
        }
    }

    pub fn organization_id(&self) -> OrganizationId {
        match *self {
            Self::Organization(organization_id)
            | Self::Environment(organization_id, _)
            | Self::Workspace(organization_id, _) => organization_id,
        }
    }

    /// The scope's keyspace. Every adapter stores a scope's values under this
    /// prefix and nowhere else.
    pub fn storage_prefix(&self) -> String {
        let org = |id: OrganizationId| id.uuid().simple().to_string();
        match *self {
            Self::Organization(organization_id) => format!("org/{}/o", org(organization_id)),
            Self::Environment(organization_id, environment) => {
                format!("org/{}/e/{}", org(organization_id), environment.as_str())
            }
            Self::Workspace(organization_id, workspace_id) => {
                format!("org/{}/w/{}", org(organization_id), workspace_id.uuid().simple())
            }
        }
    }

    /// The full key for a path in this scope.
    pub fn storage_key(&self, path: &ObjectPath) -> String {
        format!("{}/{}", self.storage_prefix(), path.as_str())
    }
}

/// A name inside a scope: 1–100 characters of `[A-Za-z0-9._:-]`, and never
/// `.` or `..`, so it's safe as a path segment too. Built by the server.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(String);

/// A key or path that isn't allowed. It never echoes the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyNameError {
    #[error("a name is 1–100 characters of letters, digits, `.`, `_`, `:` or `-`, and not `.` or `..`")]
    InvalidName,
    #[error("a path is 1–16 names separated by `/`, 300 characters at most")]
    InvalidPath,
}

/// The longest name. Small enough that an adapter can escape every character
/// and add a marker and still fit a filesystem's 255-byte name limit.
pub const MAX_NAME: usize = 100;

/// The longest path, for the same reason against a 1024-byte path limit.
pub const MAX_PATH: usize = 300;

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

impl Name {
    pub fn new(name: &str) -> Result<Self, KeyNameError> {
        if valid_name(name) {
            Ok(Self(name.to_owned()))
        } else {
            Err(KeyNameError::InvalidName)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An object's path inside its scope: up to 16 [`Name`]s separated by `/`,
/// [`MAX_PATH`] characters at most. No
/// leading or trailing `/` and no empty segment, so a path can't climb out of
/// its scope. Case is significant: `Report.pdf` and `report.pdf` are two objects.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectPath(String);

impl ObjectPath {
    pub fn new(path: &str) -> Result<Self, KeyNameError> {
        let segments: Vec<&str> = path.split('/').collect();
        let valid =
            path.len() <= MAX_PATH && segments.len() <= 16 && segments.iter().all(|segment| valid_name(segment));
        if valid {
            Ok(Self(path.to_owned()))
        } else {
            Err(KeyNameError::InvalidPath)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn lays_scopes_out_in_disjoint_keyspaces() {
        let org = OrganizationId::generate();
        let ws = WorkspaceId::generate();
        let hex = |uuid: uuid::Uuid| uuid.simple().to_string();
        assert_eq!(
            Scope::Organization(org).storage_prefix(),
            format!("org/{}/o", hex(org.uuid()))
        );
        assert_eq!(
            Scope::Environment(org, Environment::Sandbox).storage_prefix(),
            format!("org/{}/e/sandbox", hex(org.uuid()))
        );
        assert_eq!(
            Scope::Workspace(org, ws).storage_prefix(),
            format!("org/{}/w/{}", hex(org.uuid()), hex(ws.uuid()))
        );
        assert_eq!(Scope::Workspace(org, ws).organization_id(), org);
    }

    /// Any path spelled to imitate another scope's keyspace still lands in its own.
    #[test]
    fn never_gives_two_scoped_paths_the_same_key() {
        let org = OrganizationId::generate();
        let other = OrganizationId::generate();
        let ws = WorkspaceId::generate();
        let ws_hex = ws.uuid().simple().to_string();
        let org_hex = org.uuid().simple().to_string();
        let scopes = [
            Scope::Organization(org),
            Scope::Organization(other),
            Scope::Environment(org, Environment::Sandbox),
            Scope::Environment(org, Environment::Production),
            Scope::Workspace(org, ws),
            Scope::Workspace(other, ws),
        ];
        let paths = [
            "secret".to_owned(),
            format!("w/{ws_hex}/secret"),
            "e/sandbox/secret".to_owned(),
            "o/secret".to_owned(),
            format!("org/{org_hex}/w/{ws_hex}/secret"),
            format!("w/{ws_hex}"),
        ];
        let mut keys = HashSet::new();
        for scope in &scopes {
            for path in &paths {
                let key = scope.storage_key(&ObjectPath::new(path).expect("a valid path"));
                assert!(key.starts_with(&format!("{}/", scope.storage_prefix())));
                assert!(keys.insert(key), "{scope:?} {path}");
            }
        }
    }

    #[test]
    fn accepts_only_plain_names_and_paths() {
        assert!(Name::new("run:031DCp7X.events-1_a").is_ok());
        assert!(Name::new(".hidden").is_ok());
        assert!(Name::new(&"x".repeat(MAX_NAME)).is_ok());
        for bad in ["", ".", "..", "a/b", "a b", "ä", "a\0b", &"x".repeat(MAX_NAME + 1)] {
            assert_eq!(Name::new(bad), Err(KeyNameError::InvalidName), "{bad:?}");
        }
        assert!(ObjectPath::new("icons/app_1/main.png").is_ok());
        let deep = vec!["a"; 17].join("/");
        // 98 + 99 + 99 + 1 characters and three separators: exactly 300, then one more.
        let at_limit = format!("{}/{}/{}/d", "a".repeat(98), "b".repeat(99), "c".repeat(99));
        assert_eq!(at_limit.len(), MAX_PATH);
        assert!(ObjectPath::new(&at_limit).is_ok());
        assert_eq!(ObjectPath::new(&format!("{at_limit}d")), Err(KeyNameError::InvalidPath));
        for bad in [
            "",
            "/a",
            "a/",
            "a//b",
            ".",
            "..",
            "a/../b",
            "a/./b",
            "../etc/passwd",
            "a\\b",
            "a b/c",
            &deep,
        ] {
            assert_eq!(ObjectPath::new(bad), Err(KeyNameError::InvalidPath), "{bad:?}");
        }
    }
}
