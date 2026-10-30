//! The `AuditStore` suite: an organization's events only, newest first,
//! filtered by workspace and paged by keyset without repeating or skipping
//! one; an event placed in a workspace takes that workspace's environment,
//! and only from a workspace of its own organization; a deleted workspace's
//! history stays; and a store's change records its event with it, never
//! without it.
//!
//! A change made in a workspace records its event in that workspace, never
//! elsewhere: any other place fails the change.
//!
//! A change made outside the database (a memory a memory kind keeps) is
//! recorded on its own, as it says, and only in a workspace of its own
//! organization.
//!
//! `prepare` makes an organization exist. Every other suite's changes record
//! [`by_suite`], or [`by_suite_in`] their workspace.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::audit::{
    AuditAction, AuditActor, AuditEntry, AuditError, AuditEvent, AuditFilter, AuditObject, AuditPlace, AuditStore,
    AuditTarget,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{MemoryId, OrganizationId, UserId, WorkspaceId};
use zupflow_core::workspaces::{NewWorkspace, Workspace, WorkspaceError, WorkspaceStore};

pub type Prepare = Arc<dyn Fn(OrganizationId) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// What the suites' changes record: Zupflow itself, on the organization.
pub fn by_suite<T>(_: &T) -> AuditEntry {
    AuditEntry::new(
        AuditActor::System {
            name: "Conformance".to_owned(),
        },
        AuditAction::ToolPolicyUpdated,
        AuditTarget::tool_policy(),
        AuditPlace::Organization,
    )
}

/// What the suites' changes in a workspace record: Zupflow itself, there.
pub fn by_suite_in(workspace_id: WorkspaceId) -> AuditEntry {
    AuditEntry {
        place: AuditPlace::Workspace(workspace_id),
        ..by_suite(&())
    }
}

/// The stores the suite needs: the log, and the workspaces it records.
pub struct Stores<'a> {
    pub audit: &'a dyn AuditStore,
    pub workspaces: &'a dyn WorkspaceStore,
}

pub async fn run(stores: Stores<'_>, prepare: Prepare) {
    records_and_lists_newest_first(&stores, &prepare).await;
    lists_only_its_own_organization(&stores, &prepare).await;
    pages_without_repeating_or_skipping(&stores, &prepare).await;
    refuses_a_cursor_it_did_not_issue(&stores, &prepare).await;
    keeps_a_deleted_workspaces_history(&stores, &prepare).await;
    records_with_the_change_only(&stores, &prepare).await;
    records_a_change_made_elsewhere(&stores, &prepare).await;
}

async fn organization(prepare: &Prepare) -> OrganizationId {
    let organization_id = OrganizationId::generate();
    prepare(organization_id).await;
    organization_id
}

fn person(name: &str) -> AuditActor {
    AuditActor::User {
        id: UserId::generate(),
        name: name.to_owned(),
    }
}

fn entry(action: AuditAction, target: AuditTarget, place: AuditPlace) -> AuditEntry {
    AuditEntry::new(person("Jordan"), action, target, place)
}

async fn workspace(stores: &Stores<'_>, org: OrganizationId, slug: &str, environment: Environment) -> Workspace {
    stores
        .workspaces
        .create_workspace(
            org,
            WorkspaceId::generate(),
            &NewWorkspace {
                name: slug.to_uppercase(),
                slug: slug.to_owned(),
                environment,
                external_id: None,
            },
            &|workspace: &Workspace| {
                entry(
                    AuditAction::WorkspaceCreated,
                    AuditTarget::new(AuditObject::Workspace(workspace.id), &workspace.name),
                    AuditPlace::Workspace(workspace.id),
                )
            },
        )
        .await
        .unwrap()
}

/// Records `entry` through a store's change (a new workspace's, the entry
/// placed wherever it says), and returns the event as listed.
async fn record(stores: &Stores<'_>, org: OrganizationId, entry: AuditEntry) -> AuditEvent {
    let id = WorkspaceId::generate();
    stores
        .workspaces
        .create_workspace(
            org,
            id,
            &NewWorkspace {
                name: "Seed".to_owned(),
                slug: format!("seed-{}", id.uuid().simple()),
                environment: Environment::Sandbox,
                external_id: None,
            },
            &|_: &Workspace| entry.clone(),
        )
        .await
        .unwrap();
    all(stores, org, &everything()).await.remove(0)
}

async fn all(stores: &Stores<'_>, org: OrganizationId, filter: &AuditFilter) -> Vec<AuditEvent> {
    stores
        .audit
        .list_audit_events(org, filter, None, 100)
        .await
        .unwrap()
        .items
}

fn everything() -> AuditFilter {
    AuditFilter::default()
}

async fn records_and_lists_newest_first(stores: &Stores<'_>, prepare: &Prepare) {
    let org = organization(prepare).await;
    let acme = workspace(stores, org, "acme", Environment::Production).await;
    let policy = record(
        stores,
        org,
        entry(
            AuditAction::ToolPolicyUpdated,
            AuditTarget::tool_policy(),
            AuditPlace::Organization,
        ),
    )
    .await;
    assert_eq!(policy.organization_id, org);
    assert_eq!((policy.workspace_id, policy.environment), (None, None));
    let settings = record(
        stores,
        org,
        entry(
            AuditAction::ConnectSettingsUpdated,
            AuditTarget::connect_settings(Environment::Sandbox),
            AuditPlace::Environment(Environment::Sandbox),
        ),
    )
    .await;
    assert_eq!(settings.environment, Some(Environment::Sandbox));
    assert_eq!(settings.target.name, "Connect (sandbox)");

    let listed = all(stores, org, &everything()).await;
    let actions: Vec<AuditAction> = listed.iter().map(|event| event.action).collect();
    assert_eq!(
        actions,
        vec![
            AuditAction::ConnectSettingsUpdated,
            AuditAction::ToolPolicyUpdated,
            AuditAction::WorkspaceCreated
        ],
        "newest first"
    );
    assert_eq!(listed[0], settings, "listed as recorded");
    let created = &listed[2];
    assert_eq!(created.workspace_id, Some(acme.id));
    assert_eq!(
        created.environment,
        Some(Environment::Production),
        "a workspace's event takes its environment"
    );
    let named = created.workspace.as_ref().unwrap();
    assert_eq!(
        (named.id, named.name.as_str(), named.slug.as_str(), named.environment),
        (acme.id, "ACME", "acme", Environment::Production)
    );
    assert_eq!(created.actor.name(), "Jordan");
    assert_eq!(created.target.object, AuditObject::Workspace(acme.id));

    // The workspace's name follows the workspace; the target's is as it was.
    stores
        .workspaces
        .update_workspace(org, acme.id, Some("Acme Inc"), None, &|_| by_suite_in(acme.id))
        .await
        .unwrap();
    let created = all(stores, org, &everything())
        .await
        .into_iter()
        .find(|event| event.action == AuditAction::WorkspaceCreated)
        .unwrap();
    assert_eq!(created.workspace.unwrap().name, "Acme Inc");
    assert_eq!(created.target.name, "ACME");
}

async fn lists_only_its_own_organization(stores: &Stores<'_>, prepare: &Prepare) {
    let (a, b) = (organization(prepare).await, organization(prepare).await);
    let mine = workspace(stores, a, "mine", Environment::Sandbox).await;
    let theirs = workspace(stores, b, "theirs", Environment::Sandbox).await;
    // Both organizations name a workspace with the same slug.
    let twin = workspace(stores, b, "mine", Environment::Sandbox).await;

    let listed = all(stores, a, &everything()).await;
    assert_eq!(listed.len(), 1);
    assert!(listed.iter().all(|event| event.organization_id == a));
    assert_eq!(listed[0].workspace_id, Some(mine.id));

    for foreign in [theirs.id, twin.id] {
        let filtered = stores
            .audit
            .list_audit_events(
                a,
                &AuditFilter {
                    workspace_id: Some(foreign),
                },
                None,
                100,
            )
            .await
            .unwrap();
        assert_eq!(
            (filtered.items.len(), filtered.total),
            (0, 0),
            "a filter naming another organization's workspace finds nothing"
        );
    }
    let own = stores
        .audit
        .list_audit_events(
            b,
            &AuditFilter {
                workspace_id: Some(theirs.id),
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(own.items.len(), 1, "within its own organization, the filter finds it");

    // A change in one workspace records there, and nowhere else: another
    // organization's workspace, another of its own, or no workspace at all
    // fails the change.
    let sibling = workspace(stores, a, "sibling", Environment::Sandbox).await;
    for place in [
        AuditPlace::Workspace(theirs.id),
        AuditPlace::Workspace(sibling.id),
        AuditPlace::Organization,
        AuditPlace::Environment(Environment::Sandbox),
    ] {
        let placed = stores
            .workspaces
            .update_workspace(a, mine.id, Some("Moved"), None, &|_: &Workspace| {
                entry(AuditAction::WorkspaceUpdated, AuditTarget::tool_policy(), place)
            })
            .await;
        assert_eq!(placed, Err(WorkspaceError::Unavailable), "{place:?}");
    }
    assert_eq!(all(stores, a, &everything()).await.len(), 2, "nothing written");
    assert_eq!(
        all(stores, b, &everything()).await.len(),
        2,
        "nothing written there either"
    );
    assert_eq!(
        stores
            .workspaces
            .find_workspace(a, mine.id)
            .await
            .unwrap()
            .map(|w| w.name),
        Some("MINE".to_owned()),
        "and the change is rolled back"
    );
    // Nor can a change of one organization record in another's workspace.
    let refused = stores
        .workspaces
        .create_workspace(
            a,
            WorkspaceId::generate(),
            &NewWorkspace {
                name: "Sneaky".to_owned(),
                slug: "sneaky".to_owned(),
                environment: Environment::Sandbox,
                external_id: None,
            },
            &|_: &Workspace| {
                entry(
                    AuditAction::WorkspaceCreated,
                    AuditTarget::tool_policy(),
                    AuditPlace::Workspace(theirs.id),
                )
            },
        )
        .await;
    assert_eq!(
        refused,
        Err(WorkspaceError::Unavailable),
        "the change fails with its event"
    );
    assert_eq!(all(stores, a, &everything()).await.len(), 2);
    assert!(
        stores
            .workspaces
            .list_workspaces(a, &Default::default(), None, 100)
            .await
            .unwrap()
            .items
            .iter()
            .all(|workspace| workspace.slug != "sneaky"),
        "and the change is rolled back"
    );
}

async fn pages_without_repeating_or_skipping(stores: &Stores<'_>, prepare: &Prepare) {
    let org = organization(prepare).await;
    let other = organization(prepare).await;
    for _ in 0..3 {
        record(stores, other, by_suite(&())).await;
    }
    let mut recorded = Vec::new();
    for n in 0..7 {
        let event = record(
            stores,
            org,
            entry(
                AuditAction::ApiKeyCreated,
                AuditTarget::new(AuditObject::ToolPolicy, format!("Key {n}")),
                AuditPlace::Environment(Environment::Production),
            ),
        )
        .await;
        recorded.push(event.id);
    }
    recorded.reverse();

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = stores
            .audit
            .list_audit_events(org, &everything(), cursor.as_deref(), 3)
            .await
            .unwrap();
        pages += 1;
        assert_eq!(page.total, 7, "the total counts every match, on every page");
        assert!(page.items.len() <= 3);
        seen.extend(page.items.iter().map(|event| event.id));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, recorded, "every event once, newest first");
    assert_eq!(seen.iter().collect::<HashSet<_>>().len(), 7);

    // Limits clamp to 1..=100.
    let one = stores
        .audit
        .list_audit_events(org, &everything(), None, 0)
        .await
        .unwrap();
    assert_eq!(one.items.len(), 1);
    assert!(one.next_cursor.is_some());

    // A cursor names a position only: used in another organization, it
    // starts there among that organization's own events.
    let elsewhere = stores
        .audit
        .list_audit_events(other, &everything(), one.next_cursor.as_deref(), 100)
        .await
        .unwrap();
    assert!(elsewhere.items.iter().all(|event| event.organization_id == other));
}

async fn refuses_a_cursor_it_did_not_issue(stores: &Stores<'_>, prepare: &Prepare) {
    let org = organization(prepare).await;
    for cursor in ["", "not-a-cursor", "MTIzCndzXzAzMURDcDdYTVo5QUhJQjJ1RjZRZ1M"] {
        assert_eq!(
            stores
                .audit
                .list_audit_events(org, &everything(), Some(cursor), 10)
                .await
                .map(|page| page.items.len()),
            Err(AuditError::InvalidCursor),
            "{cursor:?}"
        );
    }
}

async fn keeps_a_deleted_workspaces_history(stores: &Stores<'_>, prepare: &Prepare) {
    let org = organization(prepare).await;
    let doomed = workspace(stores, org, "doomed", Environment::Sandbox).await;
    let kept = workspace(stores, org, "kept", Environment::Sandbox).await;
    stores
        .workspaces
        .update_workspace(org, doomed.id, None, None, &|_: &Workspace| {
            entry(
                AuditAction::AgentCreated,
                AuditTarget::new(AuditObject::Workspace(doomed.id), "Helper"),
                AuditPlace::Workspace(doomed.id),
            )
        })
        .await
        .unwrap();
    stores
        .workspaces
        .delete_workspace(org, doomed.id, &|workspace: &Workspace| {
            entry(
                AuditAction::WorkspaceDeleted,
                AuditTarget::new(AuditObject::Workspace(workspace.id), &workspace.name),
                AuditPlace::Workspace(workspace.id),
            )
        })
        .await
        .unwrap();

    let history = stores
        .audit
        .list_audit_events(
            org,
            &AuditFilter {
                workspace_id: Some(doomed.id),
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(history.total, 3, "a deleted workspace's events stay");
    assert_eq!(
        history.items.iter().map(|event| event.action).collect::<Vec<_>>(),
        vec![
            AuditAction::WorkspaceDeleted,
            AuditAction::AgentCreated,
            AuditAction::WorkspaceCreated
        ],
        "its deletion among them, newest first"
    );
    for event in &history.items {
        assert_eq!(event.workspace_id, Some(doomed.id), "still naming it");
        assert_eq!(event.workspace, None, "but no longer describing it");
        assert_eq!(event.environment, Some(Environment::Sandbox));
    }
    let listed = all(stores, org, &everything()).await;
    let deleted = &listed[0];
    assert_eq!(
        deleted.action,
        AuditAction::WorkspaceDeleted,
        "the deletion is recorded"
    );
    assert_eq!(deleted.target.name, "DOOMED", "named as it was");
    assert!(
        listed
            .iter()
            .any(|event| event.workspace_id == Some(kept.id) && event.workspace.is_some()),
        "a workspace that stays is still described"
    );

    // A deletion that fails records nothing.
    assert_eq!(
        stores
            .workspaces
            .delete_workspace(org, doomed.id, &|_| by_suite_in(doomed.id))
            .await,
        Err(WorkspaceError::NotFound)
    );
    assert_eq!(all(stores, org, &everything()).await.len(), listed.len());
}

async fn records_with_the_change_only(stores: &Stores<'_>, prepare: &Prepare) {
    let org = organization(prepare).await;
    let acme = workspace(stores, org, "acme", Environment::Production).await;
    let before = all(stores, org, &everything()).await.len();
    let taken = stores
        .workspaces
        .create_workspace(
            org,
            WorkspaceId::generate(),
            &NewWorkspace {
                name: "Acme again".to_owned(),
                slug: "acme".to_owned(),
                environment: Environment::Production,
                external_id: None,
            },
            &by_suite,
        )
        .await;
    assert_eq!(taken, Err(WorkspaceError::SlugTaken));
    assert_eq!(
        all(stores, org, &everything()).await.len(),
        before,
        "a refused change records nothing"
    );
    let renamed = stores
        .workspaces
        .update_workspace(org, acme.id, Some("Acme Inc"), None, &|workspace: &Workspace| {
            entry(
                AuditAction::WorkspaceUpdated,
                AuditTarget::new(AuditObject::Workspace(workspace.id), &workspace.name),
                AuditPlace::Workspace(workspace.id),
            )
        })
        .await
        .unwrap();
    let latest = all(stores, org, &everything()).await;
    assert_eq!(latest.len(), before + 1);
    assert_eq!(latest[0].action, AuditAction::WorkspaceUpdated);
    assert_eq!(latest[0].target.name, renamed.name, "named as changed");
}

async fn records_a_change_made_elsewhere(stores: &Stores<'_>, prepare: &Prepare) {
    let (org, other) = (organization(prepare).await, organization(prepare).await);
    let sandbox = workspace(stores, org, "sandbox", Environment::Sandbox).await;
    let doomed = workspace(stores, org, "doomed", Environment::Sandbox).await;
    let theirs = workspace(stores, other, "theirs", Environment::Production).await;
    let saved = entry(
        AuditAction::MemorySaved,
        AuditTarget::new(AuditObject::Memory(MemoryId::generate()), "Helper · k7m2qd"),
        AuditPlace::Workspace(sandbox.id),
    );
    stores.audit.record(org, &saved).await.unwrap();
    let recorded = all(stores, org, &everything()).await.remove(0);
    assert_eq!(
        (
            recorded.organization_id,
            recorded.workspace_id,
            recorded.environment,
            &recorded.actor,
            recorded.action,
            &recorded.target,
        ),
        (
            org,
            Some(sandbox.id),
            Some(Environment::Sandbox),
            &saved.actor,
            saved.action,
            &saved.target,
        ),
        "as it says, with its workspace's environment"
    );
    let policy = entry(
        AuditAction::ToolPolicyUpdated,
        AuditTarget::tool_policy(),
        AuditPlace::Environment(Environment::Production),
    );
    stores.audit.record(org, &policy).await.unwrap();
    let recorded = all(stores, org, &everything()).await.remove(0);
    assert_eq!(
        (recorded.action, recorded.workspace_id, recorded.environment),
        (AuditAction::ToolPolicyUpdated, None, Some(Environment::Production)),
        "or in an environment"
    );

    // Never in another organization's workspace, nor in one that's gone.
    stores
        .workspaces
        .delete_workspace(org, doomed.id, &|_: &Workspace| by_suite_in(doomed.id))
        .await
        .unwrap();
    let (mine, theirs_before) = (
        all(stores, org, &everything()).await.len(),
        all(stores, other, &everything()).await.len(),
    );
    for place in [
        AuditPlace::Workspace(theirs.id),
        AuditPlace::Workspace(doomed.id),
        AuditPlace::Workspace(WorkspaceId::generate()),
    ] {
        assert_eq!(
            stores.audit.record(org, &AuditEntry { place, ..saved.clone() }).await,
            Err(AuditError::NotFound),
            "{place:?}"
        );
    }
    assert_eq!(all(stores, org, &everything()).await.len(), mine, "nothing written");
    assert_eq!(
        all(stores, other, &everything()).await.len(),
        theirs_before,
        "nothing written there either"
    );
}
