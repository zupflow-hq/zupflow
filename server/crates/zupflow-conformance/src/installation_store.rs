//! The `InstallationStore` suite (decisions 0031 and 0034): installations
//! reached only through their own workspace, of an app the workspace may
//! install, their credentials kept and returned exactly and gone once
//! disconnected or deleted, and the catalog-wide list paged without repeats.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, a Sandbox and a
//! Production workspace, a catalog app, and an app of the Sandbox workspace's own.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{
    ConnectedAccount, Installation, InstallationError, InstallationStatus, InstallationStore, InstalledApp,
    NewCredential, NewInstallation,
};
use zupflow_core::keys::{Sealed, WrappedKey};

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    pub sandbox: WorkspaceId,
    pub production: WorkspaceId,
    pub catalog_app: AppId,
    /// The Sandbox workspace's own app.
    pub own_app: AppId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        sandbox: WorkspaceId::generate(),
        production: WorkspaceId::generate(),
        catalog_app: AppId::generate(),
        own_app: AppId::generate(),
    };
    prepare(tenant).await;
    tenant
}

/// Opaque to the store: kept and returned exactly.
fn credential(marker: u8) -> NewCredential {
    NewCredential {
        sealed: Sealed {
            wrapped_key: WrappedKey {
                key_id: format!("key-{marker}"),
                bytes: vec![marker; 40],
            },
            nonce: [marker; 12],
            ciphertext: vec![marker; 32],
        },
        version: 1,
    }
}

fn new(app: InstalledApp, label: &str, marker: u8) -> NewInstallation {
    NewInstallation {
        app,
        label: label.to_owned(),
        configuration_version: 3,
        account: ConnectedAccount {
            external_id: format!("acct-{marker}"),
            display_name: "Jordan".to_owned(),
            email: Some("jordan@example.com".to_owned()),
        },
        granted_scopes: vec!["read".to_owned()],
        expires_at: None,
        credential: credential(marker),
    }
}

async fn install(
    store: &dyn InstallationStore,
    tenant: &Tenant,
    workspace: WorkspaceId,
    app: InstalledApp,
) -> Installation {
    store
        .create_installation(
            tenant.organization_id,
            workspace,
            InstallationId::generate(),
            &new(app, "Main", 1),
        )
        .await
        .unwrap()
}

pub async fn run(store: &dyn InstallationStore, prepare: Prepare) {
    creates_and_finds_within_its_workspace(store, &prepare).await;
    refuses_an_app_the_workspace_cannot_install(store, &prepare).await;
    lists_its_own_workspace_newest_first(store, &prepare).await;
    changes_only_within_its_workspace(store, &prepare).await;
    forgets_the_credential_when_disconnected_or_deleted(store, &prepare).await;
    lists_a_catalog_apps_installations_page_by_page(store, &prepare).await;
    counts_installations_per_app(store, &prepare).await;
}

async fn creates_and_finds_within_its_workspace(store: &dyn InstallationStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let created = store
        .create_installation(
            a.organization_id,
            a.sandbox,
            InstallationId::generate(),
            &new(InstalledApp::Catalog(a.catalog_app), "Marketing", 7),
        )
        .await
        .unwrap();
    assert_eq!(
        (created.organization_id, created.workspace_id, created.app),
        (a.organization_id, a.sandbox, InstalledApp::Catalog(a.catalog_app))
    );
    assert_eq!(
        (created.label.as_str(), created.status),
        ("Marketing", InstallationStatus::Connected)
    );
    assert_eq!(created.configuration_version, 3);
    let connection = created.connection.clone().unwrap();
    assert_eq!(
        (connection.account.external_id.as_str(), connection.credential_version),
        ("acct-7", 1)
    );
    assert_eq!(connection.granted_scopes, ["read"]);
    assert_eq!(
        store
            .find_installation(a.organization_id, a.sandbox, created.id)
            .await
            .unwrap(),
        Some(created.clone())
    );
    assert_eq!(
        store
            .find_credential(a.organization_id, a.sandbox, created.id)
            .await
            .unwrap(),
        Some(credential(7)),
        "the sealed bytes come back exactly"
    );
    for (organization, workspace, case) in [
        (a.organization_id, a.production, "another workspace of the organization"),
        (
            b.organization_id,
            a.sandbox,
            "the right workspace under another organization",
        ),
        (b.organization_id, b.sandbox, "another organization's workspace"),
    ] {
        assert_eq!(
            store
                .find_installation(organization, workspace, created.id)
                .await
                .unwrap(),
            None,
            "{case}"
        );
        assert_eq!(
            store
                .find_credential(organization, workspace, created.id)
                .await
                .unwrap(),
            None,
            "{case}"
        );
    }
    let own = install(store, &a, a.sandbox, InstalledApp::Workspace(a.own_app)).await;
    assert_eq!(own.app, InstalledApp::Workspace(a.own_app));
}

async fn refuses_an_app_the_workspace_cannot_install(store: &dyn InstallationStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    for (workspace, app, case) in [
        (
            a.production,
            InstalledApp::Workspace(a.own_app),
            "another workspace's own app",
        ),
        (
            a.sandbox,
            InstalledApp::Catalog(b.catalog_app),
            "another organization's catalog app",
        ),
        (
            a.sandbox,
            InstalledApp::Workspace(b.own_app),
            "another organization's workspace app",
        ),
        (
            a.sandbox,
            InstalledApp::Workspace(a.catalog_app),
            "a catalog app as a workspace app",
        ),
        (
            a.sandbox,
            InstalledApp::Catalog(a.own_app),
            "a workspace app as a catalog app",
        ),
        (
            b.sandbox,
            InstalledApp::Catalog(a.catalog_app),
            "into another organization's workspace",
        ),
        (
            b.sandbox,
            InstalledApp::Catalog(b.catalog_app),
            "another organization's workspace and app, named under this one",
        ),
    ] {
        let result = store
            .create_installation(
                a.organization_id,
                workspace,
                InstallationId::generate(),
                &new(app, "X", 1),
            )
            .await;
        assert_eq!(result, Err(InstallationError::NotFound), "{case}");
    }
    assert_eq!(
        store.list_installations(a.organization_id, a.sandbox).await.unwrap(),
        [],
        "nothing was written"
    );
    assert_eq!(
        store.list_installations(a.organization_id, a.production).await.unwrap(),
        []
    );
}

async fn lists_its_own_workspace_newest_first(store: &dyn InstallationStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let first = install(store, &a, a.sandbox, InstalledApp::Catalog(a.catalog_app)).await;
    let second = install(store, &a, a.sandbox, InstalledApp::Workspace(a.own_app)).await;
    install(store, &a, a.production, InstalledApp::Catalog(a.catalog_app)).await;
    install(store, &b, b.sandbox, InstalledApp::Catalog(b.catalog_app)).await;
    let listed: Vec<InstallationId> = store
        .list_installations(a.organization_id, a.sandbox)
        .await
        .unwrap()
        .iter()
        .map(|installation| installation.id)
        .collect();
    // Newest first; within one instant, the later ID (IDs rise with time).
    assert_eq!(listed, [second.id, first.id], "newest first");
    assert_eq!(
        store.list_installations(b.organization_id, a.sandbox).await.unwrap(),
        []
    );
}

async fn changes_only_within_its_workspace(store: &dyn InstallationStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let installation = install(store, &a, a.sandbox, InstalledApp::Catalog(a.catalog_app)).await;
    for (organization, workspace) in [
        (a.organization_id, a.production),
        (b.organization_id, a.sandbox),
        (b.organization_id, b.sandbox),
    ] {
        assert_eq!(
            store
                .rename_installation(organization, workspace, installation.id, "Stolen")
                .await,
            Err(InstallationError::NotFound)
        );
        assert_eq!(
            store
                .disconnect_installation(organization, workspace, installation.id)
                .await,
            Err(InstallationError::NotFound)
        );
        assert_eq!(
            store
                .delete_installation(organization, workspace, installation.id)
                .await,
            Err(InstallationError::NotFound)
        );
    }
    assert_eq!(
        store
            .find_installation(a.organization_id, a.sandbox, installation.id)
            .await
            .unwrap(),
        Some(installation.clone()),
        "untouched"
    );
    assert!(
        store
            .find_credential(a.organization_id, a.sandbox, installation.id)
            .await
            .unwrap()
            .is_some()
    );
    let renamed = store
        .rename_installation(a.organization_id, a.sandbox, installation.id, "Renamed")
        .await
        .unwrap();
    assert_eq!(
        (renamed.label.as_str(), renamed.status),
        ("Renamed", InstallationStatus::Connected)
    );
    assert!(renamed.updated_at >= installation.updated_at);
}

async fn forgets_the_credential_when_disconnected_or_deleted(store: &dyn InstallationStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let kept = install(store, &a, a.sandbox, InstalledApp::Catalog(a.catalog_app)).await;
    let disconnected = install(store, &a, a.sandbox, InstalledApp::Workspace(a.own_app)).await;
    let deleted = install(store, &a, a.production, InstalledApp::Catalog(a.catalog_app)).await;

    let after = store
        .disconnect_installation(a.organization_id, a.sandbox, disconnected.id)
        .await
        .unwrap();
    assert_eq!(after.status, InstallationStatus::Disconnected);
    assert_eq!(
        after.connection, disconnected.connection,
        "the account it was is still shown"
    );
    assert_eq!(
        store
            .find_credential(a.organization_id, a.sandbox, disconnected.id)
            .await
            .unwrap(),
        None
    );

    store
        .delete_installation(a.organization_id, a.production, deleted.id)
        .await
        .unwrap();
    assert_eq!(
        store
            .find_installation(a.organization_id, a.production, deleted.id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .find_credential(a.organization_id, a.production, deleted.id)
            .await
            .unwrap(),
        None
    );
    assert!(
        store
            .find_credential(a.organization_id, a.sandbox, kept.id)
            .await
            .unwrap()
            .is_some(),
        "a sibling keeps its credential"
    );
}

async fn lists_a_catalog_apps_installations_page_by_page(store: &dyn InstallationStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let mut created = HashSet::new();
    for workspace in [a.sandbox, a.production, a.sandbox, a.production, a.sandbox] {
        created.insert(
            install(store, &a, workspace, InstalledApp::Catalog(a.catalog_app))
                .await
                .id,
        );
    }
    // Neither the workspace's own app nor another organization's counts.
    install(store, &a, a.sandbox, InstalledApp::Workspace(a.own_app)).await;
    install(store, &b, b.sandbox, InstalledApp::Catalog(b.catalog_app)).await;

    let mut seen = HashSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store
            .list_app_installations(a.organization_id, a.catalog_app, None, cursor.as_deref(), 2)
            .await
            .unwrap();
        assert_eq!(page.total, 5);
        assert!(page.items.len() <= 2);
        for item in &page.items {
            assert_eq!(item.installation.app, InstalledApp::Catalog(a.catalog_app));
            assert!(seen.insert(item.installation.id), "never repeated");
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, created, "never skipped");

    let sandbox = store
        .list_app_installations(a.organization_id, a.catalog_app, Some(Environment::Sandbox), None, 100)
        .await
        .unwrap();
    assert_eq!(sandbox.total, 3);
    assert!(
        sandbox
            .items
            .iter()
            .all(|item| item.environment == Environment::Sandbox && item.installation.workspace_id == a.sandbox)
    );
    assert_eq!(
        store
            .list_app_installations(b.organization_id, a.catalog_app, None, None, 100)
            .await
            .unwrap()
            .total,
        0,
        "not through another organization"
    );
    assert!(matches!(
        store
            .list_app_installations(a.organization_id, a.catalog_app, None, Some("not-a-cursor"), 10)
            .await,
        Err(InstallationError::InvalidCursor)
    ));
}

async fn counts_installations_per_app(store: &dyn InstallationStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    install(store, &a, a.sandbox, InstalledApp::Catalog(a.catalog_app)).await;
    install(store, &a, a.production, InstalledApp::Catalog(a.catalog_app)).await;
    install(store, &a, a.sandbox, InstalledApp::Workspace(a.own_app)).await;
    install(store, &b, b.sandbox, InstalledApp::Catalog(b.catalog_app)).await;
    let mut counts = store.count_by_app(a.organization_id, None).await.unwrap();
    counts.sort();
    let mut expected = vec![(a.catalog_app, 2), (a.own_app, 1)];
    expected.sort();
    assert_eq!(counts, expected);
    let mut sandbox = store
        .count_by_app(a.organization_id, Some(Environment::Sandbox))
        .await
        .unwrap();
    sandbox.sort();
    assert_eq!(sandbox, expected.iter().map(|&(app, _)| (app, 1)).collect::<Vec<_>>());
    assert_eq!(
        store
            .count_by_app(a.organization_id, Some(Environment::Production))
            .await
            .unwrap(),
        [(a.catalog_app, 1)]
    );
}
