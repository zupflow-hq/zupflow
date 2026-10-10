//! The in-memory cache and the local object store pass their conformance
//! suites, and keep their own limits and promises.

use std::path::PathBuf;
use std::time::Duration;

use zupflow_basic::memory_cache::MemoryCache;
use zupflow_core::ids::OrganizationId;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::storage::{Cache, StorageError};

fn name(value: &str) -> Name {
    Name::new(value).expect("a valid name")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_cache_conforms() {
    zupflow_conformance::cache::run(std::sync::Arc::new(MemoryCache::default())).await;
}

#[tokio::test]
async fn memory_cache_stays_within_its_bounds() {
    let cache = MemoryCache::new(2, 10, 8);
    let scope = Scope::Organization(OrganizationId::generate());
    let hour = Duration::from_secs(3600);
    assert_eq!(
        cache.put(&scope, &name("big"), vec![0; 9], hour).await,
        Err(StorageError::TooLarge)
    );

    cache
        .put(&scope, &name("soon"), b"1".to_vec(), Duration::from_secs(60))
        .await
        .expect("put");
    cache
        .put(&scope, &name("later"), b"2".to_vec(), hour)
        .await
        .expect("put");
    cache
        .put(&scope, &name("latest"), b"3".to_vec(), hour)
        .await
        .expect("put");
    // Full at two entries: the one closest to expiring made room.
    assert_eq!(cache.get(&scope, &name("soon")).await, Ok(None));
    assert!(cache.get(&scope, &name("later")).await.expect("get").is_some());

    // Ten bytes in all: an eight-byte value pushes out what's needed, soonest first.
    cache
        .put(&scope, &name("bulky"), vec![0; 8], hour * 2)
        .await
        .expect("put");
    assert_eq!(cache.get(&scope, &name("later")).await, Ok(None));
    assert!(cache.get(&scope, &name("bulky")).await.expect("get").is_some());

    // Overwriting doesn't count twice.
    cache
        .put(&scope, &name("bulky"), vec![1; 8], hour * 2)
        .await
        .expect("put");
    assert_eq!(cache.get(&scope, &name("bulky")).await.expect("get"), Some(vec![1; 8]));
}

#[cfg(unix)]
mod local_files {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use zupflow_basic::local_files::LocalObjectStore;
    use zupflow_core::ids::WorkspaceId;
    use zupflow_core::scope::ObjectPath;
    use zupflow_core::storage::ObjectStore;

    /// A fresh directory for one test, removed when it ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("zupflow-objects-{}", WorkspaceId::generate())))
        }

        fn root(&self) -> PathBuf {
            self.0.canonicalize().expect("the root exists")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn path(value: &str) -> ObjectPath {
        ObjectPath::new(value).expect("a valid path")
    }

    #[tokio::test]
    async fn conforms() {
        let dir = TempDir::new();
        let store = LocalObjectStore::new(&dir.0, 8 << 20).expect("a writable directory");
        zupflow_conformance::object_store::run(&store).await;
    }

    #[tokio::test]
    async fn lays_objects_out_by_scope_privately() {
        let dir = TempDir::new();
        let store = LocalObjectStore::new(&dir.0, 16).expect("a writable directory");
        let org = OrganizationId::generate();
        let ws = WorkspaceId::generate();
        store
            .put(&Scope::Workspace(org, ws), &path("icons/Main.png"), b"png".to_vec())
            .await
            .expect("put");
        store
            .put(&Scope::Organization(org), &path("icons/Main.png"), b"org".to_vec())
            .await
            .expect("put");
        let root = dir.root();
        let (org_hex, ws_hex) = (org.uuid().simple().to_string(), ws.uuid().simple().to_string());
        let in_workspace = root.join(format!("org/{org_hex}/w/{ws_hex}/icons/^main.png~"));
        assert!(in_workspace.is_file(), "{in_workspace:?}");
        assert!(root.join(format!("org/{org_hex}/o/icons/^main.png~")).is_file());
        assert_eq!(
            std::fs::metadata(&in_workspace).expect("a file").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&root).expect("the root").permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::read_dir(root.join("tmp")).expect("tmp exists").count(),
            0,
            "no temporary file is left"
        );
        assert_eq!(
            store.put(&Scope::Organization(org), &path("big"), vec![0; 17]).await,
            Err(StorageError::TooLarge)
        );
    }

    #[tokio::test]
    async fn sweeps_leftover_temporary_files_at_startup() {
        let dir = TempDir::new();
        drop(LocalObjectStore::new(&dir.0, 16).expect("a writable directory"));
        std::fs::write(dir.root().join("tmp/leftover"), b"half").expect("a file");
        drop(LocalObjectStore::new(&dir.0, 16).expect("reopens"));
        assert_eq!(std::fs::read_dir(dir.root().join("tmp")).expect("tmp").count(), 0);
    }

    /// Links planted in the tree, at the object or at a directory above it,
    /// are refused: nothing outside the root is read, written or deleted.
    #[tokio::test]
    async fn never_follows_a_symbolic_link() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        std::fs::create_dir_all(&outside.0).expect("a directory");
        std::fs::write(outside.0.join("secret~"), b"outside").expect("a file");
        let store = LocalObjectStore::new(&dir.0, 1 << 20).expect("a writable directory");
        let org = OrganizationId::generate();
        let scope = Scope::Organization(org);
        store.put(&scope, &path("placed"), b"x".to_vec()).await.expect("put");
        let objects = dir.root().join(format!("org/{}/o", org.uuid().simple()));

        // The object itself replaced by a link out of the root.
        std::fs::remove_file(objects.join("placed~")).expect("remove");
        std::os::unix::fs::symlink(outside.0.join("secret~"), objects.join("placed~")).expect("a link");
        assert_eq!(store.get(&scope, &path("placed")).await, Ok(None));

        // A directory above an object replaced by a link out of the root.
        std::os::unix::fs::symlink(&outside.0, objects.join("dir")).expect("a link");
        assert_eq!(
            store.get(&scope, &path("dir/secret")).await,
            Err(StorageError::Unavailable)
        );
        assert_eq!(
            store.put(&scope, &path("dir/written"), b"x".to_vec()).await,
            Err(StorageError::Unavailable)
        );
        assert_eq!(
            store.delete(&scope, &path("dir/secret")).await,
            Err(StorageError::Unavailable)
        );
        assert!(outside.0.join("secret~").is_file(), "the outside file survives");
        assert!(!outside.0.join("written~").exists(), "nothing was written outside");

        // A scope whose directory is a link isn't followed by delete_scope either.
        let ws = WorkspaceId::generate();
        std::fs::create_dir_all(dir.root().join(format!("org/{}/w", org.uuid().simple()))).expect("dirs");
        std::os::unix::fs::symlink(
            &outside.0,
            dir.root()
                .join(format!("org/{}/w/{}", org.uuid().simple(), ws.uuid().simple())),
        )
        .expect("a link");
        assert_eq!(
            store.delete_scope(&Scope::Workspace(org, ws)).await,
            Err(StorageError::Unavailable)
        );
        assert!(
            outside.0.join("secret~").is_file(),
            "delete_scope removed nothing outside"
        );
    }
}

#[cfg(unix)]
#[test]
fn refuses_a_temporary_directory_that_is_a_link() {
    let root = std::env::temp_dir().join(format!(
        "zupflow-objects-{}",
        zupflow_core::ids::WorkspaceId::generate()
    ));
    let outside = std::env::temp_dir().join(format!(
        "zupflow-outside-{}",
        zupflow_core::ids::WorkspaceId::generate()
    ));
    std::fs::create_dir_all(&root).expect("a directory");
    std::fs::create_dir_all(&outside).expect("a directory");
    std::fs::write(outside.join("victim"), b"keep").expect("a file");
    std::os::unix::fs::symlink(&outside, root.join("tmp")).expect("a link");
    let opened = zupflow_basic::local_files::LocalObjectStore::new(&root, 16);
    assert!(opened.is_err(), "a linked tmp is refused");
    assert!(outside.join("victim").is_file(), "and nothing it points to was swept");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}
