//! The `IdempotencyStore` suite: a key runs once and then replays exactly; a
//! different request with it is refused; one in flight is refused until its
//! lock lapses, and then taken over; only the lock's holder saves or forgets
//! it; a key lasts its lifetime; and keys of different callers, workspaces,
//! organizations and users never meet.
//!
//! `prepare` makes an organization with two workspaces, and a user, exist.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::environment::Environment;
use zupflow_core::idempotency::{
    Begun, IdempotencyCaller, IdempotencyScope, IdempotencyStore, KEY_TTL_MS, LOCK_MS, LockToken,
};
use zupflow_core::ids::{ApiKeyId, OrganizationId, UserId, WorkspaceId};
use zupflow_core::time::Timestamp;

pub type Prepare =
    Arc<dyn Fn(OrganizationId, [WorkspaceId; 2], UserId) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

struct Fixture {
    org: OrganizationId,
    ws: WorkspaceId,
    sibling: WorkspaceId,
    user: UserId,
    other_user: UserId,
}

async fn fixture(prepare: &Prepare) -> Fixture {
    let fixture = Fixture {
        org: OrganizationId::generate(),
        ws: WorkspaceId::generate(),
        sibling: WorkspaceId::generate(),
        user: UserId::generate(),
        other_user: UserId::generate(),
    };
    prepare(fixture.org, [fixture.ws, fixture.sibling], fixture.user).await;
    fixture
}

impl Fixture {
    /// The fixture's user, as a member acting in `environment`.
    fn member(&self, environment: Option<Environment>) -> IdempotencyCaller {
        IdempotencyCaller::Member {
            user_id: self.user,
            environment,
        }
    }

    fn organization_route(&self, caller: IdempotencyCaller) -> IdempotencyScope {
        IdempotencyScope::Organization {
            organization_id: self.org,
            workspace_id: None,
            caller,
        }
    }

    fn workspace_route(&self, workspace_id: WorkspaceId, caller: IdempotencyCaller) -> IdempotencyScope {
        IdempotencyScope::Organization {
            organization_id: self.org,
            workspace_id: Some(workspace_id),
            caller,
        }
    }
}

fn at(millis: i64) -> Timestamp {
    Timestamp::from_millis(1_800_000_000_000 + millis)
}

fn lock(begun: Begun) -> LockToken {
    match begun {
        Begun::Run(lock) => lock,
        other => panic!("expected to run, got {other:?}"),
    }
}

pub async fn run(store: &dyn IdempotencyStore, prepare: Prepare) {
    runs_once_then_replays(store, &prepare).await;
    refuses_a_key_reused_for_another_request(store, &prepare).await;
    holds_a_key_while_it_runs_then_lets_it_be_taken_over(store, &prepare).await;
    forgets_an_abandoned_key(store, &prepare).await;
    lets_a_key_expire(store, &prepare).await;
    keeps_every_scope_apart(store, &prepare).await;
    takes_over_one_scope_only(store, &prepare).await;
}

async fn runs_once_then_replays(store: &dyn IdempotencyStore, prepare: &Prepare) {
    let fixture = fixture(prepare).await;
    for scope in [
        fixture.organization_route(fixture.member(None)),
        IdempotencyScope::User { user_id: fixture.user },
    ] {
        let held = lock(store.begin(&scope, "k1", "fp", at(0)).await.unwrap());
        store
            .complete(&scope, "k1", &held, 201, Some(b"{\"ok\":1}".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            store.begin(&scope, "k1", "fp", at(10)).await.unwrap(),
            Begun::Replay {
                status: 201,
                body: Some(b"{\"ok\":1}".to_vec())
            },
            "{scope:?}"
        );
        // A route that shows a secret once saves the status, never the body.
        let held = lock(store.begin(&scope, "secret", "fp", at(0)).await.unwrap());
        store.complete(&scope, "secret", &held, 201, None).await.unwrap();
        assert_eq!(
            store.begin(&scope, "secret", "fp", at(10)).await.unwrap(),
            Begun::Replay {
                status: 201,
                body: None
            }
        );
        // An error is saved too.
        let held = lock(store.begin(&scope, "error", "fp", at(0)).await.unwrap());
        store
            .complete(&scope, "error", &held, 422, Some(b"{}".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            store.begin(&scope, "error", "fp", at(10)).await.unwrap(),
            Begun::Replay {
                status: 422,
                body: Some(b"{}".to_vec())
            }
        );
    }
}

async fn refuses_a_key_reused_for_another_request(store: &dyn IdempotencyStore, prepare: &Prepare) {
    let fixture = fixture(prepare).await;
    let scope = fixture.workspace_route(fixture.ws, fixture.member(Some(Environment::Sandbox)));
    let held = lock(store.begin(&scope, "k", "fp-1", at(0)).await.unwrap());
    assert_eq!(
        store.begin(&scope, "k", "fp-2", at(1)).await.unwrap(),
        Begun::Reused,
        "while running"
    );
    store.complete(&scope, "k", &held, 200, Some(vec![])).await.unwrap();
    assert_eq!(
        store.begin(&scope, "k", "fp-2", at(2)).await.unwrap(),
        Begun::Reused,
        "once done"
    );
}

async fn holds_a_key_while_it_runs_then_lets_it_be_taken_over(store: &dyn IdempotencyStore, prepare: &Prepare) {
    let fixture = fixture(prepare).await;
    let scope = fixture.organization_route(IdempotencyCaller::ApiKey {
        api_key_id: ApiKeyId::generate(),
        environment: Environment::Production,
    });
    let first = lock(store.begin(&scope, "k", "fp", at(0)).await.unwrap());
    assert_eq!(
        store.begin(&scope, "k", "fp", at(LOCK_MS - 1)).await.unwrap(),
        Begun::InProgress
    );
    let second = lock(store.begin(&scope, "k", "fp", at(LOCK_MS + 1)).await.unwrap());
    assert_ne!(first, second, "a new lock");
    // The lapsed request finishes late: its lock no longer holds the key.
    store
        .complete(&scope, "k", &first, 201, Some(b"late".to_vec()))
        .await
        .unwrap();
    store.abandon(&scope, "k", &first).await.unwrap();
    assert_eq!(
        store.begin(&scope, "k", "fp", at(LOCK_MS + 2)).await.unwrap(),
        Begun::InProgress,
        "the late request neither saved nor forgot it"
    );
    store
        .complete(&scope, "k", &second, 201, Some(b"on time".to_vec()))
        .await
        .unwrap();
    assert_eq!(
        store.begin(&scope, "k", "fp", at(LOCK_MS + 3)).await.unwrap(),
        Begun::Replay {
            status: 201,
            body: Some(b"on time".to_vec())
        }
    );
}

async fn forgets_an_abandoned_key(store: &dyn IdempotencyStore, prepare: &Prepare) {
    let fixture = fixture(prepare).await;
    for scope in [
        fixture.workspace_route(fixture.ws, fixture.member(Some(Environment::Sandbox))),
        IdempotencyScope::User { user_id: fixture.user },
    ] {
        let held = lock(store.begin(&scope, "5xx", "fp", at(0)).await.unwrap());
        store.abandon(&scope, "5xx", &held).await.unwrap();
        lock(store.begin(&scope, "5xx", "another fp", at(1)).await.unwrap());
    }
}

async fn lets_a_key_expire(store: &dyn IdempotencyStore, prepare: &Prepare) {
    let fixture = fixture(prepare).await;
    for scope in [
        fixture.organization_route(fixture.member(None)),
        IdempotencyScope::User { user_id: fixture.user },
    ] {
        let held = lock(store.begin(&scope, "old", "fp", at(0)).await.unwrap());
        store.complete(&scope, "old", &held, 201, Some(vec![])).await.unwrap();
        assert!(matches!(
            store.begin(&scope, "old", "other", at(KEY_TTL_MS - 1)).await.unwrap(),
            Begun::Reused
        ));
        lock(store.begin(&scope, "old", "other", at(KEY_TTL_MS + 1)).await.unwrap());
    }
}

async fn keeps_every_scope_apart(store: &dyn IdempotencyStore, prepare: &Prepare) {
    let (fixture, other) = (fixture(prepare).await, fixture(prepare).await);
    let sandbox = Some(Environment::Sandbox);
    let key = ApiKeyId::generate();
    let scopes = [
        // One member, one organization: an organization route and each workspace.
        fixture.organization_route(fixture.member(None)),
        fixture.workspace_route(fixture.ws, fixture.member(sandbox)),
        fixture.workspace_route(fixture.sibling, fixture.member(sandbox)),
        // The same workspace: the member in the other environment, and another member.
        fixture.workspace_route(fixture.ws, fixture.member(Some(Environment::Production))),
        fixture.workspace_route(
            fixture.ws,
            IdempotencyCaller::Member {
                user_id: fixture.other_user,
                environment: sandbox,
            },
        ),
        // An API key, in each environment, and a member whose UUID happens to equal it.
        fixture.workspace_route(
            fixture.ws,
            IdempotencyCaller::ApiKey {
                api_key_id: key,
                environment: Environment::Sandbox,
            },
        ),
        fixture.organization_route(IdempotencyCaller::ApiKey {
            api_key_id: key,
            environment: Environment::Production,
        }),
        // Differing from the key's Sandbox scope above by the caller's kind alone.
        fixture.workspace_route(
            fixture.ws,
            IdempotencyCaller::Member {
                user_id: UserId::from_uuid(key.uuid()),
                environment: sandbox,
            },
        ),
        // The same member in another organization.
        other.organization_route(fixture.member(None)),
        other.workspace_route(other.ws, fixture.member(sandbox)),
        // Creating an organization: each user's own key space.
        IdempotencyScope::User { user_id: fixture.user },
        IdempotencyScope::User { user_id: other.user },
    ];
    // The same key everywhere, each with its own request: each runs.
    let mut locks = Vec::new();
    for (index, scope) in scopes.iter().enumerate() {
        let begun = store
            .begin(scope, "shared", &format!("fp-{index}"), at(0))
            .await
            .unwrap();
        locks.push(lock(begun));
    }
    for (index, (scope, held)) in scopes.iter().zip(&locks).enumerate() {
        store
            .complete(scope, "shared", held, 201, Some(vec![u8::try_from(index).unwrap()]))
            .await
            .unwrap();
    }
    for (index, scope) in scopes.iter().enumerate() {
        assert_eq!(
            store
                .begin(scope, "shared", &format!("fp-{index}"), at(1))
                .await
                .unwrap(),
            Begun::Replay {
                status: 201,
                body: Some(vec![u8::try_from(index).unwrap()])
            },
            "{scope:?} replays its own answer only"
        );
    }
}

/// Taking over a lapsed key in one scope leaves the same key held in another
/// scope alone: its holder still completes it.
async fn takes_over_one_scope_only(store: &dyn IdempotencyStore, prepare: &Prepare) {
    let (fixture, other) = (fixture(prepare).await, fixture(prepare).await);
    let sandbox = Some(Environment::Sandbox);
    let pairs = [
        // Differing only by workspace.
        (
            fixture.workspace_route(fixture.ws, fixture.member(sandbox)),
            fixture.workspace_route(fixture.sibling, fixture.member(sandbox)),
        ),
        // Only by organization, the same caller.
        (
            fixture.organization_route(fixture.member(None)),
            other.organization_route(fixture.member(None)),
        ),
        // Only by the caller's environment.
        (
            fixture.workspace_route(fixture.ws, fixture.member(sandbox)),
            fixture.workspace_route(fixture.ws, fixture.member(Some(Environment::Production))),
        ),
        // Only by who the caller is: another member.
        (
            fixture.workspace_route(fixture.ws, fixture.member(sandbox)),
            fixture.workspace_route(
                fixture.ws,
                IdempotencyCaller::Member {
                    user_id: fixture.other_user,
                    environment: sandbox,
                },
            ),
        ),
        // Only by the caller's kind: an API key with the member's UUID.
        (
            fixture.workspace_route(fixture.ws, fixture.member(sandbox)),
            fixture.workspace_route(
                fixture.ws,
                IdempotencyCaller::ApiKey {
                    api_key_id: ApiKeyId::from_uuid(fixture.user.uuid()),
                    environment: Environment::Sandbox,
                },
            ),
        ),
    ];
    for (index, (lapsing, held)) in pairs.iter().enumerate() {
        let key = format!("takeover-{index}");
        lock(store.begin(lapsing, &key, "fp", at(0)).await.unwrap());
        let holder = lock(store.begin(held, &key, "fp", at(LOCK_MS)).await.unwrap());
        // The first lapses and is taken over; the second is still within its lock.
        lock(store.begin(lapsing, &key, "fp", at(LOCK_MS + 1)).await.unwrap());
        store
            .complete(held, &key, &holder, 201, Some(b"kept".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            store.begin(held, &key, "fp", at(LOCK_MS + 2)).await.unwrap(),
            Begun::Replay {
                status: 201,
                body: Some(b"kept".to_vec())
            },
            "{held:?} kept its own lock"
        );
    }
}
