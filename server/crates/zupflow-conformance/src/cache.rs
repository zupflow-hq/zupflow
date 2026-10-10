//! The `Cache` suite: entries round-trip, expire, and never cross a scope.
//! Organization, environment and workspace scopes are separate namespaces, and
//! deleting one scope removes nothing outside it. `add_if_absent` stores
//! only over an absent or expired entry, and of concurrent callers exactly one
//! wins.

use std::sync::Arc;
use std::time::Duration;

use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::storage::Cache;

const HOUR: Duration = Duration::from_secs(3600);

fn name(value: &str) -> Name {
    Name::new(value).unwrap()
}

pub async fn run(cache: Arc<dyn Cache>) {
    let shared = Arc::clone(&cache);
    let cache = cache.as_ref();
    round_trips(cache).await;
    adds_only_when_absent(cache).await;
    adds_once_among_concurrent_callers(shared).await;
    keeps_scopes_apart(cache).await;
    resists_keys_that_spell_another_keyspace(cache).await;
    deletes_only_inside_a_scope(cache).await;
    expires(cache).await;
}

async fn round_trips(cache: &dyn Cache) {
    let scope = Scope::Organization(OrganizationId::generate());
    let key = name("models");
    assert_eq!(cache.get(&scope, &key).await.unwrap(), None, "a missing entry is None");
    cache.put(&scope, &key, b"first".to_vec(), HOUR).await.unwrap();
    assert_eq!(cache.get(&scope, &key).await.unwrap().as_deref(), Some(&b"first"[..]));
    cache.put(&scope, &key, b"second".to_vec(), HOUR).await.unwrap();
    assert_eq!(
        cache.get(&scope, &key).await.unwrap().as_deref(),
        Some(&b"second"[..]),
        "put overwrites"
    );
    cache.delete(&scope, &key).await.unwrap();
    assert_eq!(cache.get(&scope, &key).await.unwrap(), None, "delete removes");
    cache.delete(&scope, &key).await.unwrap();
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

async fn keeps_scopes_apart(cache: &dyn Cache) {
    let key = name("shared-name");
    let scopes = scopes();
    for (scope, value) in &scopes {
        cache.put(scope, &key, value.to_vec(), HOUR).await.unwrap();
    }
    for (scope, value) in &scopes {
        assert_eq!(
            cache.get(scope, &key).await.unwrap().as_deref(),
            Some(*value),
            "{scope:?} reads its own value"
        );
    }
    cache.delete(&scopes[2].0, &key).await.unwrap();
    for (index, (scope, value)) in scopes.iter().enumerate() {
        let expected = (index != 2).then_some(*value);
        assert_eq!(
            cache.get(scope, &key).await.unwrap().as_deref(),
            expected,
            "a delete stays in its scope"
        );
    }
}

/// From an organization's own scope and from its Sandbox, keys spelled like a
/// workspace's or an environment's entries, in the common flat layouts, reach
/// nothing of them.
async fn resists_keys_that_spell_another_keyspace(cache: &dyn Cache) {
    let org = OrganizationId::generate();
    let ws = WorkspaceId::generate();
    cache
        .put(&Scope::Workspace(org, ws), &name("shared"), b"workspace".to_vec(), HOUR)
        .await
        .unwrap();
    cache
        .put(
            &Scope::Environment(org, Environment::Production),
            &name("shared"),
            b"production".to_vec(),
            HOUR,
        )
        .await
        .unwrap();
    let imitations = crate::imitations::imitations(org, ws, "shared");
    for listener in [Scope::Organization(org), Scope::Environment(org, Environment::Sandbox)] {
        for imitation in &imitations {
            assert_eq!(
                cache.get(&listener, &name(imitation)).await.unwrap(),
                None,
                "{listener:?} through {imitation}"
            );
        }
    }
}

async fn deletes_only_inside_a_scope(cache: &dyn Cache) {
    let key = name("entry");
    let scopes = scopes();
    for (scope, value) in &scopes {
        cache.put(scope, &key, value.to_vec(), HOUR).await.unwrap();
    }
    // Deleting workspace 1 of organization a leaves its organization, its sibling and b's same-UUID workspace.
    cache.delete_scope(&scopes[2].0).await.unwrap();
    for (index, (scope, value)) in scopes.iter().enumerate() {
        let expected = (index != 2).then_some(*value);
        assert_eq!(
            cache.get(scope, &key).await.unwrap().as_deref(),
            expected,
            "deleting a workspace's scope"
        );
    }
    // An organization's own scope holds only its entries, never its environments' or workspaces'.
    cache.delete_scope(&scopes[0].0).await.unwrap();
    assert_eq!(cache.get(&scopes[0].0, &key).await.unwrap(), None);
    for index in [1, 3, 4, 5, 6] {
        let (scope, value) = &scopes[index];
        assert_eq!(
            cache.get(scope, &key).await.unwrap().as_deref(),
            Some(*value),
            "{scope:?} keeps its entry"
        );
    }
    cache.delete_scope(&scopes[5].0).await.unwrap();
    assert_eq!(cache.get(&scopes[5].0, &key).await.unwrap(), None);
    assert_eq!(
        cache.get(&scopes[6].0, &key).await.unwrap().as_deref(),
        Some(scopes[6].1),
        "Production keeps its entry"
    );
}

/// Whole seconds, with wide margins: a loaded machine or a networked backend
/// mustn't make this fail, and backends that count TTLs in seconds qualify.
async fn expires(cache: &dyn Cache) {
    let scope = Scope::Organization(OrganizationId::generate());
    let key = name("short-lived");
    cache
        .put(&scope, &key, b"value".to_vec(), Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        cache.get(&scope, &key).await.unwrap().is_some(),
        "an entry is there before it expires"
    );
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(cache.get(&scope, &key).await.unwrap(), None, "an expired entry is gone");
}

async fn adds_only_when_absent(cache: &dyn Cache) {
    let scope = Scope::Organization(OrganizationId::generate());
    let key = name("once");
    assert!(
        cache
            .add_if_absent(&scope, &key, b"first".to_vec(), HOUR)
            .await
            .unwrap(),
        "an absent key is stored"
    );
    assert!(
        !cache
            .add_if_absent(&scope, &key, b"second".to_vec(), HOUR)
            .await
            .unwrap(),
        "a live entry refuses"
    );
    assert_eq!(
        cache.get(&scope, &key).await.unwrap().as_deref(),
        Some(&b"first"[..]),
        "and keeps its value"
    );

    // The same key in every other scope is its own.
    for (other, value) in scopes() {
        assert!(
            cache.add_if_absent(&other, &key, value.to_vec(), HOUR).await.unwrap(),
            "{other:?} is another namespace"
        );
    }
    let (a, ws) = (OrganizationId::generate(), WorkspaceId::generate());
    for scope in [
        Scope::Workspace(a, ws),
        Scope::Workspace(OrganizationId::generate(), ws),
    ] {
        assert!(cache.add_if_absent(&scope, &key, b"ws".to_vec(), HOUR).await.unwrap());
    }

    // Expired counts as absent: whole seconds, with margins, as in `expires`.
    let short = Scope::Organization(OrganizationId::generate());
    assert!(
        cache
            .add_if_absent(&short, &key, b"old".to_vec(), Duration::from_secs(2))
            .await
            .unwrap()
    );
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(
        cache.add_if_absent(&short, &key, b"new".to_vec(), HOUR).await.unwrap(),
        "an expired entry counts as absent"
    );
    assert_eq!(cache.get(&short, &key).await.unwrap().as_deref(), Some(&b"new"[..]));

    // Deleted, it can be added again.
    cache.delete(&scope, &key).await.unwrap();
    assert!(
        cache
            .add_if_absent(&scope, &key, b"again".to_vec(), HOUR)
            .await
            .unwrap()
    );
}

async fn adds_once_among_concurrent_callers(cache: Arc<dyn Cache>) {
    let scope = Scope::Workspace(OrganizationId::generate(), WorkspaceId::generate());
    let key = name("race");
    let tasks: Vec<_> = (0..16u8)
        .map(|index| {
            let cache = Arc::clone(&cache);
            let key = key.clone();
            tokio::spawn(async move { cache.add_if_absent(&scope, &key, vec![index], HOUR).await })
        })
        .collect();
    let mut won = 0;
    for task in tasks {
        if task.await.unwrap().unwrap() {
            won += 1;
        }
    }
    assert_eq!(won, 1, "exactly one concurrent caller stores");
}
