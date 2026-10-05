//! The `AppStore` suite (decision 0031): catalog apps reached only through
//! their organization, workspace apps only through their own workspace, never
//! one through the other's methods, and slugs unique within their owner.
//!
//! `prepare` makes an organization exist with the given workspaces in it, for
//! a store whose apps reference them.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::apps::{App, AppChanges, AppError, AppStore, AuthType, NewApp};
use zupflow_core::ids::{AppId, OrganizationId, WorkspaceId};

pub type Prepare =
    Arc<dyn Fn(OrganizationId, Vec<WorkspaceId>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

fn new(name: &str, slug: &str) -> NewApp {
    NewApp::blank(
        name.to_owned(),
        slug.to_owned(),
        format!("{name}, for tests."),
        AuthType::Token,
    )
}

/// An organization with two workspaces.
async fn organization(prepare: &Prepare) -> (OrganizationId, WorkspaceId, WorkspaceId) {
    let (organization_id, one, two) = (
        OrganizationId::generate(),
        WorkspaceId::generate(),
        WorkspaceId::generate(),
    );
    prepare(organization_id, vec![one, two]).await;
    (organization_id, one, two)
}

async fn catalog_app(store: &dyn AppStore, org: OrganizationId, app: NewApp) -> App {
    store
        .create_organization_app(org, AppId::generate(), &app)
        .await
        .unwrap()
}

async fn workspace_app(store: &dyn AppStore, org: OrganizationId, workspace: WorkspaceId, app: NewApp) -> App {
    store
        .create_workspace_app(org, workspace, AppId::generate(), &app)
        .await
        .unwrap()
}

pub async fn run(store: &dyn AppStore, prepare: Prepare) {
    creates_and_finds_catalog_apps_within_their_organization(store, &prepare).await;
    creates_and_finds_workspace_apps_within_their_workspace(store, &prepare).await;
    keeps_slugs_unique_within_their_owner(store, &prepare).await;
    lists_by_name_and_only_the_owners_own(store, &prepare).await;
    updates_only_within_the_owner(store, &prepare).await;
    refuses_a_workspace_of_another_organization(store, &prepare).await;
}

async fn creates_and_finds_catalog_apps_within_their_organization(store: &dyn AppStore, prepare: &Prepare) {
    let (a, a_workspace, _) = organization(prepare).await;
    let (b, _, _) = organization(prepare).await;
    let app = catalog_app(store, a, new("Linear", "linear")).await;
    assert_eq!((app.organization_id, app.owner_workspace_id, app.version), (a, None, 1));
    assert_eq!(app.created_at, app.updated_at);
    assert_eq!(
        store.find_organization_app(a, app.id).await.unwrap(),
        Some(app.clone()),
        "found as created"
    );
    assert_eq!(
        store.find_organization_app(b, app.id).await.unwrap(),
        None,
        "not through another organization"
    );
    assert_eq!(
        store.find_workspace_app(a, a_workspace, app.id).await.unwrap(),
        None,
        "not as a workspace app"
    );
}

async fn creates_and_finds_workspace_apps_within_their_workspace(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, two) = organization(prepare).await;
    let (b, b_workspace, _) = organization(prepare).await;
    let app = workspace_app(store, a, one, new("Echo", "echo")).await;
    assert_eq!((app.organization_id, app.owner_workspace_id), (a, Some(one)));
    assert_eq!(
        store.find_workspace_app(a, one, app.id).await.unwrap(),
        Some(app.clone())
    );
    for (org, workspace, case) in [
        (a, two, "another workspace of the organization"),
        (b, one, "the right workspace under another organization"),
        (b, b_workspace, "another organization's workspace"),
    ] {
        assert_eq!(
            store.find_workspace_app(org, workspace, app.id).await.unwrap(),
            None,
            "{case}"
        );
    }
    assert_eq!(
        store.find_organization_app(a, app.id).await.unwrap(),
        None,
        "not in the catalog"
    );
}

async fn keeps_slugs_unique_within_their_owner(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, two) = organization(prepare).await;
    let (b, b_workspace, _) = organization(prepare).await;
    catalog_app(store, a, new("Tools", "tools")).await;
    // The same slug in a workspace, another workspace and another organization.
    workspace_app(store, a, one, new("Tools", "tools")).await;
    workspace_app(store, a, two, new("Tools", "tools")).await;
    catalog_app(store, b, new("Tools", "tools")).await;
    workspace_app(store, b, b_workspace, new("Tools", "tools")).await;
    assert_eq!(
        store
            .create_organization_app(a, AppId::generate(), &new("Other", "tools"))
            .await,
        Err(AppError::SlugTaken)
    );
    assert_eq!(
        store
            .create_workspace_app(a, one, AppId::generate(), &new("Other", "tools"))
            .await,
        Err(AppError::SlugTaken)
    );
}

async fn lists_by_name_and_only_the_owners_own(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, two) = organization(prepare).await;
    let (b, b_workspace, _) = organization(prepare).await;
    let zeta = catalog_app(store, a, new("zeta", "zeta")).await;
    let alpha = catalog_app(store, a, new("Alpha", "alpha")).await;
    let own = workspace_app(store, a, one, new("Mine", "mine")).await;
    workspace_app(store, a, two, new("Theirs", "theirs")).await;
    catalog_app(store, b, new("Elsewhere", "elsewhere")).await;
    workspace_app(store, b, b_workspace, new("Far", "far")).await;
    let catalog: Vec<AppId> = store
        .list_organization_apps(a)
        .await
        .unwrap()
        .iter()
        .map(|app| app.id)
        .collect();
    assert_eq!(
        catalog,
        [alpha.id, zeta.id],
        "by name, case-insensitively; no workspace app"
    );
    assert_eq!(store.list_workspace_apps(a, one).await.unwrap(), [own]);
    assert_eq!(
        store.list_workspace_apps(b, one).await.unwrap(),
        [],
        "not under another organization"
    );
}

async fn updates_only_within_the_owner(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, two) = organization(prepare).await;
    let (b, b_workspace, _) = organization(prepare).await;
    let catalog = catalog_app(store, a, new("Linear", "linear")).await;
    let own = workspace_app(store, a, one, new("Echo", "echo")).await;
    let rename = AppChanges {
        name: Some("Renamed"),
        description: None,
    };

    // Through the wrong owner, nothing changes.
    for result in [
        store.update_organization_app(b, catalog.id, rename).await,
        store.update_organization_app(a, own.id, rename).await,
        store.update_workspace_app(a, one, catalog.id, rename).await,
        store.update_workspace_app(a, two, own.id, rename).await,
        store.update_workspace_app(b, b_workspace, own.id, rename).await,
        store.update_workspace_app(b, one, own.id, rename).await,
    ] {
        assert_eq!(result, Err(AppError::NotFound));
    }
    assert_eq!(
        store.find_organization_app(a, catalog.id).await.unwrap(),
        Some(catalog.clone())
    );
    assert_eq!(
        store.find_workspace_app(a, one, own.id).await.unwrap(),
        Some(own.clone())
    );

    let updated = store.update_organization_app(a, catalog.id, rename).await.unwrap();
    assert_eq!(
        (
            updated.name.as_str(),
            updated.description.as_str(),
            updated.slug.as_str()
        ),
        ("Renamed", catalog.description.as_str(), "linear")
    );
    assert!(updated.updated_at >= catalog.updated_at);
    let described = store
        .update_workspace_app(
            a,
            one,
            own.id,
            AppChanges {
                name: None,
                description: Some(""),
            },
        )
        .await
        .unwrap();
    assert_eq!((described.name.as_str(), described.description.as_str()), ("Echo", ""));
    assert_eq!(store.find_workspace_app(a, one, own.id).await.unwrap(), Some(described));
}

async fn refuses_a_workspace_of_another_organization(store: &dyn AppStore, prepare: &Prepare) {
    let (a, _, _) = organization(prepare).await;
    let (_, b_workspace, _) = organization(prepare).await;
    let result = store
        .create_workspace_app(a, b_workspace, AppId::generate(), &new("Sneaky", "sneaky"))
        .await;
    assert_eq!(result, Err(AppError::NotFound));
    assert_eq!(store.list_workspace_apps(a, b_workspace).await.unwrap(), []);
}
