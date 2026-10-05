//! The `ObjectStore` suite: objects round-trip, never cross a scope, and
//! can't be reached, overwritten or deleted from another scope by a path that
//! spells that scope's keyspace (`Scope::storage_key`'s layout).

use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::scope::{MAX_NAME, MAX_PATH, ObjectPath, Scope};
use zupflow_core::storage::ObjectStore;

fn path(value: &str) -> ObjectPath {
    ObjectPath::new(value).unwrap()
}

pub async fn run(store: &dyn ObjectStore) {
    round_trips(store).await;
    treats_paths_as_names(store).await;
    keeps_scopes_apart(store).await;
    resists_paths_that_spell_another_keyspace(store).await;
    resists_paths_from_an_environment(store).await;
    deletes_only_inside_a_scope(store).await;
}

async fn round_trips(store: &dyn ObjectStore) {
    let scope = Scope::Workspace(OrganizationId::generate(), WorkspaceId::generate());
    let icon = path("icons/app/main.png");
    assert_eq!(
        store.get(&scope, &icon).await.unwrap(),
        None,
        "a missing object is None"
    );
    store.put(&scope, &icon, b"first".to_vec()).await.unwrap();
    assert_eq!(store.get(&scope, &icon).await.unwrap().as_deref(), Some(&b"first"[..]));
    store.put(&scope, &icon, b"second".to_vec()).await.unwrap();
    assert_eq!(
        store.get(&scope, &icon).await.unwrap().as_deref(),
        Some(&b"second"[..]),
        "put overwrites"
    );

    let large: Vec<u8> = (0..1_048_576u32).map(|i| (i % 251) as u8).collect();
    store
        .put(&scope, &path("exports/large.bin"), large.clone())
        .await
        .unwrap();
    assert_eq!(
        store.get(&scope, &path("exports/large.bin")).await.unwrap(),
        Some(large),
        "1 MiB round-trips"
    );

    store.delete(&scope, &icon).await.unwrap();
    assert_eq!(store.get(&scope, &icon).await.unwrap(), None, "delete removes");
    store.delete(&scope, &icon).await.unwrap();
}

/// Paths are names, as in any object store: case is significant, a prefix of
/// one object's path can be another object, and nothing is a directory.
async fn treats_paths_as_names(store: &dyn ObjectStore) {
    let scope = Scope::Organization(OrganizationId::generate());
    store.put(&scope, &path("Report.pdf"), b"upper".to_vec()).await.unwrap();
    store.put(&scope, &path("report.pdf"), b"lower".to_vec()).await.unwrap();
    assert_eq!(
        store.get(&scope, &path("Report.pdf")).await.unwrap().as_deref(),
        Some(&b"upper"[..]),
        "case is kept"
    );
    assert_eq!(
        store.get(&scope, &path("report.pdf")).await.unwrap().as_deref(),
        Some(&b"lower"[..])
    );
    assert_eq!(
        store.get(&scope, &path("REPORT.PDF")).await.unwrap(),
        None,
        "another spelling is another name"
    );
    store.delete(&scope, &path("REPORT.PDF")).await.unwrap();
    assert!(
        store.get(&scope, &path("Report.pdf")).await.unwrap().is_some(),
        "deleting another spelling removes nothing"
    );

    store.put(&scope, &path("a"), b"parent".to_vec()).await.unwrap();
    store.put(&scope, &path("a/b"), b"child".to_vec()).await.unwrap();
    assert_eq!(
        store.get(&scope, &path("a")).await.unwrap().as_deref(),
        Some(&b"parent"[..]),
        "a path and its prefix coexist"
    );
    assert_eq!(
        store.get(&scope, &path("a/b")).await.unwrap().as_deref(),
        Some(&b"child"[..])
    );
    assert_eq!(
        store.get(&scope, &path("a/b/c")).await.unwrap(),
        None,
        "under an object is missing, not an error"
    );
    store.delete(&scope, &path("a/b/c")).await.unwrap();
    assert_eq!(store.get(&scope, &path("icons")).await.unwrap(), None);

    // The longest valid name and path, in the letters an encoding is likeliest to grow, fit.
    let mut segments: Vec<String> = Vec::new();
    while segments.iter().map(|s| s.len() + 1).sum::<usize>() + MAX_NAME <= MAX_PATH {
        segments.push("P".repeat(MAX_NAME));
    }
    let used: usize = segments.iter().map(|s| s.len() + 1).sum();
    segments.push("Q".repeat(MAX_PATH - used));
    let longest_path = segments.join("/");
    assert_eq!(longest_path.len(), MAX_PATH);
    for object in ["N".repeat(MAX_NAME), longest_path] {
        store.put(&scope, &path(&object), b"long".to_vec()).await.unwrap();
        assert_eq!(
            store.get(&scope, &path(&object)).await.unwrap().as_deref(),
            Some(&b"long"[..]),
            "{} characters",
            object.len()
        );
    }
}

/// Two organizations; one's own scope, both its environments and two of its
/// workspaces; and the same workspace UUID under the other organization.
fn scopes() -> [(Scope, &'static [u8]); 7] {
    let (a, b) = (OrganizationId::generate(), OrganizationId::generate());
    let (ws1, ws2) = (WorkspaceId::generate(), WorkspaceId::generate());
    [
        (Scope::Organization(a), b"org a"),
        (Scope::Organization(b), b"org b"),
        (Scope::Workspace(a, ws1), b"a ws1"),
        (Scope::Workspace(a, ws2), b"a ws2"),
        (Scope::Workspace(b, ws1), b"b ws1"),
        (Scope::Environment(a, Environment::Sandbox), b"a sandbox"),
        (Scope::Environment(a, Environment::Production), b"a production"),
    ]
}

async fn keeps_scopes_apart(store: &dyn ObjectStore) {
    let object = path("shared/name.txt");
    let scopes = scopes();
    for (scope, value) in &scopes {
        store.put(scope, &object, value.to_vec()).await.unwrap();
    }
    for (scope, value) in &scopes {
        assert_eq!(
            store.get(scope, &object).await.unwrap().as_deref(),
            Some(*value),
            "{scope:?} reads its own object"
        );
    }
    store.delete(&scopes[3].0, &object).await.unwrap();
    for (index, (scope, value)) in scopes.iter().enumerate() {
        let expected = (index != 3).then_some(*value);
        assert_eq!(
            store.get(scope, &object).await.unwrap().as_deref(),
            expected,
            "a delete stays in its scope"
        );
    }
}

/// From the organization's own scope, paths spelling its environments' and
/// workspaces' keyspaces (and the other common layouts) reach nothing of them,
/// to read, to overwrite or to delete.
async fn resists_paths_that_spell_another_keyspace(store: &dyn ObjectStore) {
    let org = OrganizationId::generate();
    let ws = WorkspaceId::generate();
    let (org_hex, ws_hex) = (org.uuid().simple().to_string(), ws.uuid().simple().to_string());
    let (org_uuid, ws_uuid) = (org.uuid().hyphenated().to_string(), ws.uuid().hyphenated().to_string());
    let organization = Scope::Organization(org);
    let targets = [
        (
            Scope::Workspace(org, ws),
            vec![
                format!("{ws_hex}/secret.txt"),
                format!("{ws_uuid}/secret.txt"),
                format!("{ws}/secret.txt"),
                format!("org/{org_uuid}/ws/{ws_uuid}/secret.txt"),
                format!("w/{ws_hex}/secret.txt"),
                format!("ws/{ws_hex}/secret.txt"),
                format!("ws/{ws}/secret.txt"),
                format!("ws/{ws}/objects/secret.txt"),
                format!("objects/ws/{ws}/secret.txt"),
                format!("org/{org_hex}/w/{ws_hex}/secret.txt"),
                format!("org/{org}/ws/{ws}/secret.txt"),
            ],
        ),
        (
            Scope::Environment(org, Environment::Production),
            vec![
                "e/production/secret.txt".to_owned(),
                "production/secret.txt".to_owned(),
                format!("org/{org_hex}/e/production/secret.txt"),
            ],
        ),
    ];
    for (target, imitations) in targets {
        store
            .put(&target, &path("secret.txt"), b"theirs".to_vec())
            .await
            .unwrap();
        for imitation in imitations {
            assert_eq!(
                store.get(&organization, &path(&imitation)).await.unwrap(),
                None,
                "read through {imitation}"
            );
            store
                .put(&organization, &path(&imitation), b"overwritten".to_vec())
                .await
                .unwrap();
            store.delete(&organization, &path(&imitation)).await.unwrap();
            assert_eq!(
                store.get(&target, &path("secret.txt")).await.unwrap().as_deref(),
                Some(&b"theirs"[..]),
                "write or delete through {imitation}"
            );
        }
    }
}

/// The same from an environment's scope: it reaches nothing of the
/// organization's own objects, the other environment's, or a workspace's.
async fn resists_paths_from_an_environment(store: &dyn ObjectStore) {
    let org = OrganizationId::generate();
    let ws = WorkspaceId::generate();
    let (org_hex, ws_hex) = (org.uuid().simple().to_string(), ws.uuid().simple().to_string());
    let sandbox = Scope::Environment(org, Environment::Sandbox);
    let targets = [
        (
            Scope::Organization(org),
            vec!["o/secret.txt".to_owned(), format!("org/{org_hex}/o/secret.txt")],
        ),
        (
            Scope::Environment(org, Environment::Production),
            vec!["production/secret.txt".to_owned(), "e/production/secret.txt".to_owned()],
        ),
        (
            Scope::Workspace(org, ws),
            vec![
                format!("w/{ws_hex}/secret.txt"),
                format!("{ws_hex}/secret.txt"),
                format!("{ws}/secret.txt"),
            ],
        ),
    ];
    for (target, imitations) in targets {
        store
            .put(&target, &path("secret.txt"), b"theirs".to_vec())
            .await
            .unwrap();
        for imitation in imitations {
            assert_eq!(
                store.get(&sandbox, &path(&imitation)).await.unwrap(),
                None,
                "read through {imitation}"
            );
            store
                .put(&sandbox, &path(&imitation), b"overwritten".to_vec())
                .await
                .unwrap();
            store.delete(&sandbox, &path(&imitation)).await.unwrap();
            assert_eq!(
                store.get(&target, &path("secret.txt")).await.unwrap().as_deref(),
                Some(&b"theirs"[..]),
                "write or delete through {imitation}"
            );
        }
    }
}

async fn deletes_only_inside_a_scope(store: &dyn ObjectStore) {
    let object = path("entry.bin");
    let scopes = scopes();
    for (scope, value) in &scopes {
        store.put(scope, &object, value.to_vec()).await.unwrap();
    }
    store.delete_scope(&scopes[2].0).await.unwrap();
    for (index, (scope, value)) in scopes.iter().enumerate() {
        let expected = (index != 2).then_some(*value);
        assert_eq!(
            store.get(scope, &object).await.unwrap().as_deref(),
            expected,
            "deleting a workspace's scope"
        );
    }
    store.delete_scope(&scopes[0].0).await.unwrap();
    assert_eq!(store.get(&scopes[0].0, &object).await.unwrap(), None);
    for index in [1, 3, 4, 5, 6] {
        let (scope, value) = &scopes[index];
        assert_eq!(
            store.get(scope, &object).await.unwrap().as_deref(),
            Some(*value),
            "{scope:?} keeps its object"
        );
    }
    store.delete_scope(&scopes[5].0).await.unwrap();
    assert_eq!(store.get(&scopes[5].0, &object).await.unwrap(), None);
    assert_eq!(
        store.get(&scopes[6].0, &object).await.unwrap().as_deref(),
        Some(scopes[6].1),
        "Production keeps its object"
    );
    // Deleting a scope that holds nothing is fine.
    store
        .delete_scope(&Scope::Organization(OrganizationId::generate()))
        .await
        .unwrap();
}
