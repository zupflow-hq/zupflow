//! The `WorkspaceStore` suite: workspaces reached only through their own
//! organization, slugs unique within one, filters and keyset paging that
//! never repeat or skip a row, and deletion that removes one workspace only.
//!
//! `prepare` makes an organization exist, for a store whose workspaces
//! reference their organization.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::workspaces::{
    NewWorkspace, Workspace, WorkspaceError, WorkspaceFilter, WorkspaceSort, WorkspaceStats, WorkspaceStore,
};

use crate::audit_store::{by_suite, by_suite_in};

pub type Prepare = Arc<dyn Fn(OrganizationId) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

fn new(name: &str, slug: &str, environment: Environment, external_id: Option<&str>) -> NewWorkspace {
    NewWorkspace {
        name: name.to_owned(),
        slug: slug.to_owned(),
        environment,
        external_id: external_id.map(str::to_owned),
    }
}

async fn organization(prepare: &Prepare) -> OrganizationId {
    let organization_id = OrganizationId::generate();
    prepare(organization_id).await;
    organization_id
}

async fn create(store: &dyn WorkspaceStore, org: OrganizationId, workspace: NewWorkspace) -> Workspace {
    store
        .create_workspace(org, WorkspaceId::generate(), &workspace, &by_suite)
        .await
        .unwrap()
}

pub async fn run(store: &dyn WorkspaceStore, prepare: Prepare) {
    creates_and_finds_within_its_organization(store, &prepare).await;
    keeps_slugs_unique_within_an_organization(store, &prepare).await;
    lists_only_its_own_organization(store, &prepare).await;
    filters_by_environment_and_text(store, &prepare).await;
    pages_without_repeating_or_skipping(store, &prepare).await;
    refuses_a_cursor_it_did_not_issue(store, &prepare).await;
    updates_only_within_its_organization(store, &prepare).await;
    deletes_one_workspace_only(store, &prepare).await;
}

async fn creates_and_finds_within_its_organization(store: &dyn WorkspaceStore, prepare: &Prepare) {
    let (a, b) = (organization(prepare).await, organization(prepare).await);
    let workspace = create(
        store,
        a,
        new("Acme", "acme", Environment::Production, Some("tenant-42")),
    )
    .await;
    assert_eq!(workspace.organization_id, a);
    assert_eq!(workspace.external_id.as_deref(), Some("tenant-42"));
    assert_eq!(
        store.find_workspace(a, workspace.id).await.unwrap(),
        Some(workspace.clone())
    );
    assert_eq!(
        store.find_workspace(b, workspace.id).await.unwrap(),
        None,
        "another organization never finds it"
    );
    assert_eq!(
        store.load_stats(a, workspace.id).await.unwrap(),
        WorkspaceStats::default()
    );
}

async fn keeps_slugs_unique_within_an_organization(store: &dyn WorkspaceStore, prepare: &Prepare) {
    let (a, b) = (organization(prepare).await, organization(prepare).await);
    create(store, a, new("Acme", "acme", Environment::Production, None)).await;
    assert_eq!(
        store
            .create_workspace(
                a,
                WorkspaceId::generate(),
                &new("Acme 2", "acme", Environment::Sandbox, None),
                &by_suite,
            )
            .await,
        Err(WorkspaceError::SlugTaken),
        "even in the other environment"
    );
    create(store, b, new("Acme", "acme", Environment::Production, None)).await;
}

async fn lists_only_its_own_organization(store: &dyn WorkspaceStore, prepare: &Prepare) {
    let (a, b) = (organization(prepare).await, organization(prepare).await);
    let mine = create(store, a, new("Mine", "mine", Environment::Production, None)).await;
    create(store, b, new("Theirs", "theirs", Environment::Production, None)).await;
    let page = store
        .list_workspaces(a, &WorkspaceFilter::default(), None, 100)
        .await
        .unwrap();
    assert_eq!(page.items, vec![mine]);
    assert_eq!((page.total, page.next_cursor), (1, None));
}

async fn filters_by_environment_and_text(store: &dyn WorkspaceStore, prepare: &Prepare) {
    let org = organization(prepare).await;
    let acme = create(
        store,
        org,
        new("Acme Corp", "acme", Environment::Production, Some("CUST-7")),
    )
    .await;
    let beta = create(store, org, new("Beta Labs", "beta-labs", Environment::Sandbox, None)).await;
    let gamma = create(store, org, new("Gamma", "gamma", Environment::Sandbox, Some("cust-9"))).await;
    let list = |filter: WorkspaceFilter| async move {
        store
            .list_workspaces(org, &filter, None, 100)
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|workspace| workspace.id)
            .collect::<Vec<_>>()
    };
    let sandbox = WorkspaceFilter {
        environment: Some(Environment::Sandbox),
        ..WorkspaceFilter::default()
    };
    assert_eq!(list(sandbox).await, vec![beta.id, gamma.id]);
    let text = |query: &str| WorkspaceFilter {
        query: Some(query.to_owned()),
        ..WorkspaceFilter::default()
    };
    assert_eq!(list(text("CORP")).await, vec![acme.id], "name, any case");
    assert_eq!(list(text("labs")).await, vec![beta.id], "slug");
    assert_eq!(
        list(text("cust")).await,
        vec![acme.id, gamma.id],
        "External ID, any case"
    );
    assert_eq!(list(text("%")).await, Vec::<WorkspaceId>::new(), "no wildcards");
    assert_eq!(list(text("  ")).await.len(), 3, "a blank query matches everything");
    let both = WorkspaceFilter {
        environment: Some(Environment::Production),
        query: Some("cust".to_owned()),
        sort: WorkspaceSort::Activity,
    };
    assert_eq!(list(both).await, vec![acme.id]);
}

async fn pages_without_repeating_or_skipping(store: &dyn WorkspaceStore, prepare: &Prepare) {
    let org = organization(prepare).await;
    let names = ["delta", "Alpha", "charlie", "Bravo", "echo", "alpha", "Foxtrot"];
    for (index, name) in names.iter().enumerate() {
        let environment = if index % 2 == 0 {
            Environment::Production
        } else {
            Environment::Sandbox
        };
        create(store, org, new(name, &format!("w-{index}"), environment, None)).await;
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = store
            .list_workspaces(org, &WorkspaceFilter::default(), cursor.as_deref(), 3)
            .await
            .unwrap();
        assert_eq!(
            page.total,
            7 + u64::from(pages > 0),
            "the total counts what matches now"
        );
        assert!(page.items.len() <= 3);
        seen.extend(page.items.iter().map(|workspace| workspace.name.clone()));
        pages += 1;
        if pages == 1 {
            // A workspace created mid-way, before the cursor: not repeated, not shifting the rest.
            create(store, org, new("aardvark", "late", Environment::Production, None)).await;
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let lowered: Vec<String> = seen.iter().map(|name| name.to_ascii_lowercase()).collect();
    let mut sorted = lowered.clone();
    sorted.sort();
    assert_eq!(lowered, sorted, "by name, case-insensitively");
    assert_eq!(seen.len(), 7, "every workspace once: {seen:?}");
    assert_eq!(seen.iter().collect::<HashSet<_>>().len(), 7);
}

async fn refuses_a_cursor_it_did_not_issue(store: &dyn WorkspaceStore, prepare: &Prepare) {
    let org = organization(prepare).await;
    for cursor in ["not-a-cursor", "", "!!!"] {
        assert_eq!(
            store
                .list_workspaces(org, &WorkspaceFilter::default(), Some(cursor), 10)
                .await,
            Err(WorkspaceError::InvalidCursor),
            "{cursor:?}"
        );
    }
}

async fn updates_only_within_its_organization(store: &dyn WorkspaceStore, prepare: &Prepare) {
    let (a, b) = (organization(prepare).await, organization(prepare).await);
    let workspace = create(store, a, new("Acme", "acme", Environment::Production, Some("t-1"))).await;
    assert_eq!(
        store
            .update_workspace(b, workspace.id, Some("Taken"), Some(None), &|_| by_suite_in(
                workspace.id
            ))
            .await,
        Err(WorkspaceError::NotFound)
    );
    assert_eq!(
        store.find_workspace(a, workspace.id).await.unwrap(),
        Some(workspace.clone())
    );

    let renamed = store
        .update_workspace(a, workspace.id, Some("Acme Inc"), None, &|_| by_suite_in(workspace.id))
        .await
        .unwrap();
    assert_eq!(
        (renamed.name.as_str(), renamed.external_id.as_deref()),
        ("Acme Inc", Some("t-1")),
        "the External ID is kept when not given"
    );
    let cleared = store
        .update_workspace(a, workspace.id, None, Some(None), &|_| by_suite_in(workspace.id))
        .await
        .unwrap();
    assert_eq!((cleared.name.as_str(), cleared.external_id), ("Acme Inc", None));
    let set = store
        .update_workspace(a, workspace.id, None, Some(Some("t-2")), &|_| by_suite_in(workspace.id))
        .await
        .unwrap();
    assert_eq!(set.external_id.as_deref(), Some("t-2"));
    assert_eq!(
        (set.slug, set.environment),
        (workspace.slug, workspace.environment),
        "never changed"
    );
}

async fn deletes_one_workspace_only(store: &dyn WorkspaceStore, prepare: &Prepare) {
    let (a, b) = (organization(prepare).await, organization(prepare).await);
    let doomed = create(store, a, new("Doomed", "doomed", Environment::Production, None)).await;
    let sibling = create(store, a, new("Sibling", "sibling", Environment::Production, None)).await;
    let other = create(store, b, new("Other", "other", Environment::Production, None)).await;
    assert_eq!(
        store.delete_workspace(b, doomed.id, &|_| by_suite_in(doomed.id)).await,
        Err(WorkspaceError::NotFound)
    );
    assert!(store.find_workspace(a, doomed.id).await.unwrap().is_some(), "untouched");
    store
        .delete_workspace(a, doomed.id, &|_| by_suite_in(doomed.id))
        .await
        .unwrap();
    assert_eq!(store.find_workspace(a, doomed.id).await.unwrap(), None);
    assert_eq!(
        store.delete_workspace(a, doomed.id, &|_| by_suite_in(doomed.id)).await,
        Err(WorkspaceError::NotFound)
    );
    assert_eq!(store.find_workspace(a, sibling.id).await.unwrap(), Some(sibling));
    assert_eq!(store.find_workspace(b, other.id).await.unwrap(), Some(other));
}
