//! The cache and object storage traits. Both take a [`Scope`] first; keys and
//! paths are validated types the server builds, so one tenant's entries can
//! never be named from another's scope. An implementation that maps them to
//! a flat keyspace uses [`Scope::storage_key`], whose scopes are disjoint by
//! construction, and passes its suite in `zupflow-conformance`.

use std::time::Duration;

use async_trait::async_trait;

use crate::scope::{Name, ObjectPath, Scope};

/// Why a storage operation failed. The backend's own error never crosses the
/// trait; implementations log it and answer `Unavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    #[error("the storage backend is unavailable")]
    Unavailable,
    #[error("the value is larger than this backend accepts")]
    TooLarge,
}

/// A cache of derived values: anything in it can be rebuilt, so losing an
/// entry is never an error. Entries expire after their time to live.
#[async_trait]
pub trait Cache: Send + Sync {
    async fn get(&self, scope: &Scope, key: &Name) -> Result<Option<Vec<u8>>, StorageError>;
    async fn put(&self, scope: &Scope, key: &Name, value: Vec<u8>, ttl: Duration) -> Result<(), StorageError>;
    async fn delete(&self, scope: &Scope, key: &Name) -> Result<(), StorageError>;
    /// Drops every entry of a scope. An organization scope drops only its own
    /// entries, never its environments' or workspaces'.
    async fn delete_scope(&self, scope: &Scope) -> Result<(), StorageError>;
}

/// Files and objects: icons, exports, attachments.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn put(&self, scope: &Scope, path: &ObjectPath, bytes: Vec<u8>) -> Result<(), StorageError>;
    async fn get(&self, scope: &Scope, path: &ObjectPath) -> Result<Option<Vec<u8>>, StorageError>;
    async fn delete(&self, scope: &Scope, path: &ObjectPath) -> Result<(), StorageError>;
    /// Removes everything a scope holds. An organization scope removes only
    /// its own objects, never its environments' or workspaces'. Deleting a
    /// workspace stops its writes first: a put racing the delete could
    /// otherwise recreate what was removed.
    async fn delete_scope(&self, scope: &Scope) -> Result<(), StorageError>;
}
