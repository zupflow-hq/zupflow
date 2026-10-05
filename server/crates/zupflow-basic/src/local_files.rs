//! [`LocalObjectStore`]: objects as files under one root directory. Unix only.
//!
//! Each scope lives under its keyspace from [`Scope::storage_prefix`]
//! (`org/{uuid}/o`, `org/{uuid}/e/{environment}`, `org/{uuid}/w/{uuid}`),
//! disjoint by construction. Inside it, an object path's segments become
//! directories and a file:
//!
//! - uppercase letters are written as `^` and the lowercase letter, so
//!   `Report` and `report` stay two files on a case-insensitive disk;
//! - the last segment gets a `~`, so an object and another object under its
//!   path (`a` and `a/b`) never collide. Neither `^` nor `~` can occur in a name.
//!
//! The root is created `0700` and files `0600`: it belongs to the server
//! alone. Every component of a path is checked, and a symbolic link anywhere
//! is refused rather than followed. Writes go to `tmp/`, are synced, and are
//! renamed into place, so a reader never sees half an object; leftovers from a
//! crash are swept at startup.

use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use zupflow_core::scope::{ObjectPath, Scope};
use zupflow_core::storage::{ObjectStore, StorageError};

pub struct LocalObjectStore {
    root: PathBuf,
    max_object_bytes: usize,
}

/// A path's place on disk: the directories under the root, then the file.
struct Location {
    directories: Vec<String>,
    file: String,
}

fn disk_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('^');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn location(scope: &Scope, path: &ObjectPath) -> Location {
    let mut directories: Vec<String> = scope.storage_prefix().split('/').map(str::to_owned).collect();
    let segments: Vec<&str> = path.segments().collect();
    let (last, parents) = segments
        .split_last()
        .map_or(("", &[][..]), |(last, parents)| (*last, parents));
    directories.extend(parents.iter().map(|segment| disk_segment(segment)));
    Location {
        directories,
        file: format!("{}~", disk_segment(last)),
    }
}

/// What a walk down the tree found.
enum Walk {
    /// The directory, every component a real directory.
    Directory(PathBuf),
    /// A component is missing (or a file, which the layout never produces).
    Missing,
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

impl LocalObjectStore {
    /// A store under `root`, created `0700` if missing, with leftover temporary
    /// files swept. Objects over `max_object_bytes` are refused.
    pub fn new(root: impl Into<PathBuf>, max_object_bytes: usize) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&root)?;
        let root = root.canonicalize()?;
        // An existing root keeps nothing broader than the server's own access.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        let tmp = root.join("tmp");
        create_private_dir(&tmp)?;
        // Writes and the sweep below go through `tmp`: it must be a real directory, never a link.
        if !std::fs::symlink_metadata(&tmp)?.file_type().is_dir() {
            return Err(std::io::Error::other(
                "the store's tmp directory isn't a plain directory",
            ));
        }
        for entry in std::fs::read_dir(&tmp)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(Self { root, max_object_bytes })
    }

    /// Walks the directories from the root, refusing a symbolic link anywhere.
    async fn walk(&self, scope: &Scope, directories: &[String], create: bool) -> Result<Walk, StorageError> {
        let mut current = self.root.clone();
        for directory in directories {
            current.push(directory);
            match tokio::fs::symlink_metadata(&current).await {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    tracing::warn!(event = "objects.link_refused", organization_id = %scope.organization_id());
                    return Err(StorageError::Unavailable);
                }
                Ok(_) => return Ok(Walk::Missing),
                Err(error) if error.kind() == ErrorKind::NotFound && create => {
                    let path = current.clone();
                    tokio::task::spawn_blocking(move || create_private_dir(&path))
                        .await
                        .map_err(|_| StorageError::Unavailable)?
                        .map_err(|error| unavailable(scope, "create_dir", &error))?;
                    // Created just now, or by a concurrent put: check it's a real directory.
                    let created = tokio::fs::symlink_metadata(&current)
                        .await
                        .map_err(|error| unavailable(scope, "stat", &error))?;
                    if !created.file_type().is_dir() {
                        return Err(StorageError::Unavailable);
                    }
                }
                Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                    return Ok(Walk::Missing);
                }
                Err(error) => return Err(unavailable(scope, "stat", &error)),
            }
        }
        Ok(Walk::Directory(current))
    }

    /// The file for a path when every directory above it exists, else `None`.
    async fn existing_file(&self, scope: &Scope, path: &ObjectPath) -> Result<Option<PathBuf>, StorageError> {
        let location = location(scope, path);
        match self.walk(scope, &location.directories, false).await? {
            Walk::Directory(directory) => Ok(Some(directory.join(location.file))),
            Walk::Missing => Ok(None),
        }
    }
}

fn unavailable(scope: &Scope, operation: &'static str, error: &std::io::Error) -> StorageError {
    tracing::warn!(
        event = "objects.io_failed",
        organization_id = %scope.organization_id(),
        workspace_id = scope.workspace_id().map(|id| id.to_string()),
        environment = scope.environment().map(|env| env.as_str()),
        operation,
        kind = ?error.kind(),
        os_error = error.raw_os_error(),
    );
    StorageError::Unavailable
}

#[async_trait]
impl ObjectStore for LocalObjectStore {
    async fn put(&self, scope: &Scope, path: &ObjectPath, bytes: Vec<u8>) -> Result<(), StorageError> {
        if bytes.len() > self.max_object_bytes {
            return Err(StorageError::TooLarge);
        }
        let location = location(scope, path);
        let Walk::Directory(directory) = self.walk(scope, &location.directories, true).await? else {
            return Err(StorageError::Unavailable);
        };
        let mut name = [0u8; 16];
        getrandom::fill(&mut name).map_err(|_| StorageError::Unavailable)?;
        let temporary = self
            .root
            .join("tmp")
            .join(name.iter().map(|b| format!("{b:02x}")).collect::<String>());
        let write = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            tokio::fs::rename(&temporary, directory.join(&location.file)).await
        };
        if let Err(error) = write.await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(unavailable(scope, "write", &error));
        }
        // The object is in place; syncing the directory makes the rename survive a power loss.
        let synced = async { tokio::fs::File::open(&directory).await?.sync_all().await };
        if let Err(error) = synced.await {
            let _ = unavailable(scope, "sync_directory", &error);
        }
        Ok(())
    }

    async fn get(&self, scope: &Scope, path: &ObjectPath) -> Result<Option<Vec<u8>>, StorageError> {
        let Some(file) = self.existing_file(scope, path).await? else {
            return Ok(None);
        };
        match tokio::fs::symlink_metadata(&file).await {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(metadata) if metadata.file_type().is_symlink() => {
                tracing::warn!(event = "objects.link_refused", organization_id = %scope.organization_id());
                return Ok(None);
            }
            Ok(_) => return Ok(None),
            Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => return Ok(None),
            Err(error) => return Err(unavailable(scope, "stat", &error)),
        }
        match tokio::fs::read(&file).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(None),
            Err(error) => Err(unavailable(scope, "read", &error)),
        }
    }

    async fn delete(&self, scope: &Scope, path: &ObjectPath) -> Result<(), StorageError> {
        let Some(file) = self.existing_file(scope, path).await? else {
            return Ok(());
        };
        // Removing a link removes the link itself, never what it points to.
        match tokio::fs::remove_file(&file).await {
            Ok(()) => Ok(()),
            Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(()),
            Err(error) => Err(unavailable(scope, "delete", &error)),
        }
    }

    async fn delete_scope(&self, scope: &Scope) -> Result<(), StorageError> {
        let directories: Vec<String> = scope.storage_prefix().split('/').map(str::to_owned).collect();
        // Every component checked first: removal mustn't follow a planted link above the scope.
        let Walk::Directory(directory) = self.walk(scope, &directories, false).await? else {
            return Ok(());
        };
        match tokio::fs::remove_dir_all(&directory).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(unavailable(scope, "delete_scope", &error)),
        }
    }
}
