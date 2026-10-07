//! The `ConnectStore` suite: settings per organization and environment;
//! sessions reached only through their own workspace, or by the hash of
//! their reference and nothing else; cancelling only what is pending.
//!
//! `prepare` makes a [`Tenant`] exist: an organization with a Sandbox and a
//! Production workspace, a catalog app, the Sandbox workspace's own app, and
//! an installation of the catalog app in the Sandbox workspace.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::connect::{
    ConnectError, ConnectIntent, ConnectSession, ConnectSessionStatus, ConnectStore, NewConnectSession, Reference,
    ReferenceHash, SESSION_TTL_MS,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, ConnectSessionId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::InstalledApp;
use zupflow_core::time::Timestamp;

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    pub sandbox: WorkspaceId,
    pub production: WorkspaceId,
    pub catalog_app: AppId,
    pub own_app: AppId,
    /// Of the catalog app, in the Sandbox workspace.
    pub installation: InstallationId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        sandbox: WorkspaceId::generate(),
        production: WorkspaceId::generate(),
        catalog_app: AppId::generate(),
        own_app: AppId::generate(),
        installation: InstallationId::generate(),
    };
    prepare(tenant).await;
    tenant
}

fn new(app: InstalledApp, reference: &Reference) -> NewConnectSession {
    NewConnectSession {
        environment: Environment::Sandbox,
        app,
        configuration_version: 2,
        intent: ConnectIntent::Create,
        installation_id: None,
        label: "Main".to_owned(),
        return_url: Some("https://app.example/done".to_owned()),
        requested_scopes: vec!["read".to_owned()],
        reference_hash: reference.hash(),
    }
}

async fn start(store: &dyn ConnectStore, t: &Tenant, now: Timestamp) -> (ConnectSession, Reference) {
    let reference = Reference::generate().unwrap();
    let session = store
        .create_session(
            t.organization_id,
            t.sandbox,
            ConnectSessionId::generate(),
            &new(InstalledApp::Catalog(t.catalog_app), &reference),
            now,
        )
        .await
        .unwrap();
    (session, reference)
}

pub async fn run(store: &dyn ConnectStore, prepare: Prepare) {
    keeps_settings_per_organization_and_environment(store, &prepare).await;
    finds_sessions_within_their_workspace(store, &prepare).await;
    attributes_a_session_by_its_reference_alone(store, &prepare).await;
    cancels_only_what_is_pending(store, &prepare).await;
    refuses_a_workspace_of_another_organization(store, &prepare).await;
}

async fn keeps_settings_per_organization_and_environment(store: &dyn ConnectStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let empty = store
        .find_settings(a.organization_id, Environment::Sandbox)
        .await
        .unwrap();
    assert_eq!(
        (empty.return_urls.len(), empty.updated_at),
        (0, None),
        "empty until written"
    );
    let urls = vec!["https://app.example/done".to_owned()];
    let origins = vec!["https://app.example".to_owned()];
    let saved = store
        .save_settings(a.organization_id, Environment::Sandbox, &urls, &origins)
        .await
        .unwrap();
    assert_eq!((&saved.return_urls, &saved.allowed_origins), (&urls, &origins));
    assert!(saved.updated_at.is_some());
    assert_eq!(
        store
            .find_settings(a.organization_id, Environment::Sandbox)
            .await
            .unwrap(),
        saved
    );
    for (organization, environment) in [
        (a.organization_id, Environment::Production),
        (b.organization_id, Environment::Sandbox),
    ] {
        assert_eq!(
            store
                .find_settings(organization, environment)
                .await
                .unwrap()
                .return_urls,
            Vec::<String>::new(),
            "{environment:?}"
        );
    }
    let replaced = store
        .save_settings(a.organization_id, Environment::Sandbox, &[], &origins)
        .await
        .unwrap();
    assert!(replaced.return_urls.is_empty(), "a write replaces the lists");
}

async fn finds_sessions_within_their_workspace(store: &dyn ConnectStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let now = Timestamp::now();
    let (first, _) = start(store, &a, now).await;
    assert_eq!(
        (first.organization_id, first.workspace_id, first.status),
        (a.organization_id, a.sandbox, ConnectSessionStatus::Pending)
    );
    assert_eq!(first.expires_at.millis(), now.millis() + SESSION_TTL_MS);
    let reference = Reference::generate().unwrap();
    let mut reconnect = new(InstalledApp::Catalog(a.catalog_app), &reference);
    reconnect.intent = ConnectIntent::Reconnect;
    reconnect.installation_id = Some(a.installation);
    let second = store
        .create_session(
            a.organization_id,
            a.sandbox,
            ConnectSessionId::generate(),
            &reconnect,
            now,
        )
        .await
        .unwrap();
    assert_eq!(second.installation_id, Some(a.installation));
    let own_reference = Reference::generate().unwrap();
    store
        .create_session(
            a.organization_id,
            a.sandbox,
            ConnectSessionId::generate(),
            &new(InstalledApp::Workspace(a.own_app), &own_reference),
            now,
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .find_session(a.organization_id, a.sandbox, first.id)
            .await
            .unwrap(),
        Some(first.clone())
    );
    for (organization, workspace) in [
        (a.organization_id, a.production),
        (b.organization_id, a.sandbox),
        (b.organization_id, b.sandbox),
    ] {
        assert_eq!(
            store.find_session(organization, workspace, first.id).await.unwrap(),
            None
        );
        assert!(store.list_sessions(organization, workspace).await.unwrap().is_empty());
    }
    assert_eq!(
        store.list_sessions(a.organization_id, a.sandbox).await.unwrap().len(),
        3
    );
}

async fn attributes_a_session_by_its_reference_alone(store: &dyn ConnectStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let (session, reference) = start(store, &a, Timestamp::now()).await;
    let (other, other_reference) = start(store, &b, Timestamp::now()).await;
    assert_eq!(
        store.attribute_session_by_reference(&reference.hash()).await.unwrap(),
        Some(session)
    );
    assert_eq!(
        store
            .attribute_session_by_reference(&ReferenceHash::of(other_reference.expose()))
            .await
            .unwrap(),
        Some(other)
    );
    for unknown in ["", "not-a-reference", &reference.expose()[1..]] {
        assert_eq!(
            store
                .attribute_session_by_reference(&ReferenceHash::of(unknown))
                .await
                .unwrap(),
            None,
            "{unknown:?}"
        );
    }
}

async fn cancels_only_what_is_pending(store: &dyn ConnectStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let now = Timestamp::now();
    let (session, _) = start(store, &a, now).await;
    for (organization, workspace) in [(b.organization_id, a.sandbox), (a.organization_id, a.production)] {
        assert_eq!(
            store.cancel_session(organization, workspace, session.id, now).await,
            Err(ConnectError::NotFound)
        );
    }
    let cancelled = store
        .cancel_session(a.organization_id, a.sandbox, session.id, now)
        .await
        .unwrap();
    assert_eq!(cancelled.status, ConnectSessionStatus::Cancelled);
    assert_eq!(
        store
            .cancel_session(a.organization_id, a.sandbox, session.id, now)
            .await,
        Err(ConnectError::NotPending)
    );

    // Past its expiry, a pending session reads as expired and can't be cancelled.
    let started = Timestamp::from_millis(now.millis() - SESSION_TTL_MS - 1);
    let (stale, _) = start(store, &a, started).await;
    assert_eq!(stale.status_at(now), ConnectSessionStatus::Expired);
    assert_eq!(
        store.cancel_session(a.organization_id, a.sandbox, stale.id, now).await,
        Err(ConnectError::NotPending)
    );
}

async fn refuses_a_workspace_of_another_organization(store: &dyn ConnectStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let reference = Reference::generate().unwrap();
    let result = store
        .create_session(
            a.organization_id,
            b.sandbox,
            ConnectSessionId::generate(),
            &new(InstalledApp::Catalog(a.catalog_app), &reference),
            Timestamp::now(),
        )
        .await;
    assert_eq!(result, Err(ConnectError::NotFound));
    assert_eq!(
        store.attribute_session_by_reference(&reference.hash()).await.unwrap(),
        None
    );

    // Nor a session in another environment than its workspace's.
    let mut production = new(InstalledApp::Catalog(a.catalog_app), &reference);
    production.environment = Environment::Production;
    let result = store
        .create_session(
            a.organization_id,
            a.sandbox,
            ConnectSessionId::generate(),
            &production,
            Timestamp::now(),
        )
        .await;
    assert_eq!(
        result,
        Err(ConnectError::NotFound),
        "a Sandbox workspace's session is a Sandbox session"
    );
}
