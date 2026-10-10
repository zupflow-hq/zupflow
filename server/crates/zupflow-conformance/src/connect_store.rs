//! The `ConnectStore` suite: settings per organization and environment;
//! sessions reached only through their own workspace, or by the hash of
//! their reference and nothing else; cancelling only what is pending;
//! completing a pending session once, with the installation it makes or
//! renews written in the same breath; an OAuth authorization per attempt,
//! only the newest one claimable, once, by its `state` alone, and forgotten
//! when the session finishes.
//!
//! `prepare` makes a [`Tenant`] exist: an organization with a Sandbox and a
//! Production workspace, a catalog app, the Sandbox workspace's own app, and
//! an installation of the catalog app in the Sandbox workspace.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::connect::{
    Authorization, Completion, ConnectError, ConnectIntent, ConnectSession, ConnectSessionStatus, ConnectStore,
    NewConnectSession, Reference, ReferenceHash, SESSION_TTL_MS,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, ConnectSessionId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{ConnectedAccount, InstallationStatus, InstalledApp, NewCredential};
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::time::Timestamp;

use crate::audit_store::{by_suite, by_suite_in};

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
    completes_a_pending_session_once(store, &prepare).await;
    renews_a_reconnects_installation_under_the_next_version(store, &prepare).await;
    refuses_a_workspace_of_another_organization(store, &prepare).await;
    authorizes_the_newest_attempt_once(store, &prepare).await;
    forgets_the_authorization_when_a_session_finishes(store, &prepare).await;
}

/// Opaque to the store: kept and returned exactly.
fn authorization(state: &Reference, attempt: u32) -> Authorization {
    Authorization {
        state_hash: state.hash(),
        attempt,
        sealed: Sealed {
            wrapped_key: WrappedKey {
                key_id: format!("key-{attempt}"),
                bytes: vec![2; 40],
            },
            nonce: [3; 12],
            ciphertext: vec![u8::try_from(attempt).unwrap_or(0); 24],
        },
    }
}

async fn authorizes_the_newest_attempt_once(store: &dyn ConnectStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let now = Timestamp::now();
    let (session, _) = start(store, &a, now).await;
    assert_eq!(session.authorization_attempt, 0);
    let first = Reference::generate().unwrap();
    for (organization, workspace) in [(b.organization_id, a.sandbox), (a.organization_id, a.production)] {
        assert_eq!(
            store
                .begin_authorization(organization, workspace, session.id, &authorization(&first, 1), now)
                .await,
            Err(ConnectError::NotFound)
        );
    }
    assert_eq!(
        store
            .begin_authorization(a.organization_id, a.sandbox, session.id, &authorization(&first, 2), now)
            .await,
        Err(ConnectError::AuthorizationChanged),
        "an attempt follows the one before it"
    );
    store
        .begin_authorization(a.organization_id, a.sandbox, session.id, &authorization(&first, 1), now)
        .await
        .unwrap();
    let found = store
        .find_session(a.organization_id, a.sandbox, session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (found.authorization_attempt, found.status),
        (1, ConnectSessionStatus::Pending)
    );
    let racing = Reference::generate().unwrap();
    assert_eq!(
        store
            .begin_authorization(
                a.organization_id,
                a.sandbox,
                session.id,
                &authorization(&racing, 1),
                now
            )
            .await,
        Err(ConnectError::AuthorizationChanged),
        "two starts from the same attempt: one wins"
    );

    // A newer attempt replaces the older one's `state`.
    let second = Reference::generate().unwrap();
    store
        .begin_authorization(
            a.organization_id,
            a.sandbox,
            session.id,
            &authorization(&second, 2),
            now,
        )
        .await
        .unwrap();
    for stale in [&first, &racing] {
        assert_eq!(store.claim_authorization(&stale.hash()).await.unwrap(), None);
    }
    let (claimed, claimed_authorization) = store.claim_authorization(&second.hash()).await.unwrap().unwrap();
    assert_eq!(
        (claimed.id, claimed.organization_id, claimed.workspace_id),
        (session.id, a.organization_id, a.sandbox),
        "the session it belongs to, with its scope"
    );
    assert_eq!(claimed_authorization, authorization(&second, 2), "kept exactly");
    assert_eq!(
        store.claim_authorization(&second.hash()).await.unwrap(),
        None,
        "a state is spent by its claim"
    );
    assert_eq!(
        store
            .claim_authorization(&Reference::generate().unwrap().hash())
            .await
            .unwrap(),
        None
    );

    // The session stays pending: the person can start again.
    let third = Reference::generate().unwrap();
    store
        .begin_authorization(a.organization_id, a.sandbox, session.id, &authorization(&third, 3), now)
        .await
        .unwrap();
    assert!(store.claim_authorization(&third.hash()).await.unwrap().is_some());

    // Past its expiry, a session starts nothing.
    let (stale, _) = start(store, &a, Timestamp::from_millis(now.millis() - SESSION_TTL_MS - 1)).await;
    assert_eq!(
        store
            .begin_authorization(
                a.organization_id,
                a.sandbox,
                stale.id,
                &authorization(&Reference::generate().unwrap(), 1),
                now
            )
            .await,
        Err(ConnectError::NotPending)
    );
}

async fn forgets_the_authorization_when_a_session_finishes(store: &dyn ConnectStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let now = Timestamp::now();
    let begin = |session: ConnectSessionId| async move {
        let state = Reference::generate().unwrap();
        store
            .begin_authorization(a.organization_id, a.sandbox, session, &authorization(&state, 1), now)
            .await
            .unwrap();
        state
    };

    // Failed: only within its own workspace, once, with its code.
    let (failing, _) = start(store, &a, now).await;
    let state = begin(failing.id).await;
    for (organization, workspace) in [(b.organization_id, a.sandbox), (a.organization_id, a.production)] {
        assert_eq!(
            store
                .fail_session(organization, workspace, failing.id, "consent_denied", now)
                .await,
            Err(ConnectError::NotFound)
        );
    }
    let failed = store
        .fail_session(a.organization_id, a.sandbox, failing.id, "consent_denied", now)
        .await
        .unwrap();
    assert_eq!(
        (failed.status, failed.failure_code.as_deref()),
        (ConnectSessionStatus::Failed, Some("consent_denied"))
    );
    assert_eq!(store.claim_authorization(&state.hash()).await.unwrap(), None);
    assert_eq!(
        store
            .fail_session(a.organization_id, a.sandbox, failing.id, "consent_denied", now)
            .await,
        Err(ConnectError::NotPending)
    );
    assert_eq!(
        store
            .begin_authorization(
                a.organization_id,
                a.sandbox,
                failing.id,
                &authorization(&Reference::generate().unwrap(), 2),
                now
            )
            .await,
        Err(ConnectError::NotPending)
    );

    // Cancelled, or completed another way.
    let (cancelling, _) = start(store, &a, now).await;
    let state = begin(cancelling.id).await;
    store
        .cancel_session(a.organization_id, a.sandbox, cancelling.id, now)
        .await
        .unwrap();
    assert_eq!(store.claim_authorization(&state.hash()).await.unwrap(), None);
    let (completing, _) = start(store, &a, now).await;
    let state = begin(completing.id).await;
    store
        .complete_session(
            a.organization_id,
            a.sandbox,
            completing.id,
            &completion(InstallationId::generate(), 1),
            now,
            &|_| by_suite_in(a.sandbox),
        )
        .await
        .unwrap();
    assert_eq!(store.claim_authorization(&state.hash()).await.unwrap(), None);
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
        .save_settings(a.organization_id, Environment::Sandbox, &urls, &origins, &by_suite)
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
        .save_settings(a.organization_id, Environment::Sandbox, &[], &origins, &by_suite)
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

fn completion(installation_id: InstallationId, version: u32) -> Completion {
    Completion {
        installation_id,
        configuration_version: 3,
        account: ConnectedAccount {
            external_id: "acct-1".to_owned(),
            display_name: "Account".to_owned(),
            email: None,
        },
        granted_scopes: vec!["read".to_owned()],
        expires_at: None,
        credential: NewCredential {
            sealed: Sealed {
                wrapped_key: WrappedKey {
                    key_id: "k".to_owned(),
                    bytes: vec![1; 40],
                },
                nonce: [1; 12],
                ciphertext: vec![1; 16],
            },
            version,
        },
        snapshot: crate::installation_store::snapshot(version),
    }
}

async fn completes_a_pending_session_once(store: &dyn ConnectStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let now = Timestamp::now();
    let (session, _) = start(store, &a, now).await;
    let new_installation = InstallationId::generate();
    for (organization, workspace) in [(b.organization_id, a.sandbox), (a.organization_id, a.production)] {
        assert_eq!(
            store
                .complete_session(
                    organization,
                    workspace,
                    session.id,
                    &completion(new_installation, 1),
                    now,
                    &|_| by_suite_in(workspace)
                )
                .await,
            Err(ConnectError::NotFound)
        );
    }
    assert_eq!(
        store
            .complete_session(
                a.organization_id,
                a.sandbox,
                session.id,
                &completion(new_installation, 2),
                now,
                &|_| by_suite_in(a.sandbox)
            )
            .await,
        Err(ConnectError::InstallationChanged),
        "a new installation's credential is version 1"
    );
    let (completed, installation) = store
        .complete_session(
            a.organization_id,
            a.sandbox,
            session.id,
            &completion(new_installation, 1),
            now,
            &|_| by_suite_in(a.sandbox),
        )
        .await
        .unwrap();
    assert_eq!(
        (completed.status, completed.completed_at, completed.installation_id),
        (ConnectSessionStatus::Completed, Some(now), Some(new_installation))
    );
    assert_eq!(
        (
            installation.id,
            installation.organization_id,
            installation.workspace_id,
            installation.app,
            installation.label.as_str(),
            installation.status,
            installation.configuration_version,
        ),
        (
            new_installation,
            a.organization_id,
            a.sandbox,
            InstalledApp::Catalog(a.catalog_app),
            "Main",
            InstallationStatus::Connected,
            3,
        ),
        "of the session's app, with its label"
    );
    assert_eq!(
        installation.snapshot,
        Some(crate::installation_store::snapshot(1)),
        "under the template it connected with"
    );
    let connection = installation.connection.unwrap();
    assert_eq!(
        (connection.account.external_id.as_str(), connection.credential_version),
        ("acct-1", 1)
    );
    assert_eq!(
        store
            .find_session(a.organization_id, a.sandbox, session.id)
            .await
            .unwrap(),
        Some(completed)
    );
    assert_eq!(
        store
            .complete_session(
                a.organization_id,
                a.sandbox,
                session.id,
                &completion(InstallationId::generate(), 1),
                now,
                &|_| by_suite_in(a.sandbox)
            )
            .await,
        Err(ConnectError::NotPending),
        "once"
    );

    // Past its expiry, or cancelled, a session completes nothing.
    let (stale, _) = start(store, &a, Timestamp::from_millis(now.millis() - SESSION_TTL_MS - 1)).await;
    let (cancelled, _) = start(store, &a, now).await;
    store
        .cancel_session(a.organization_id, a.sandbox, cancelled.id, now)
        .await
        .unwrap();
    for refused in [stale.id, cancelled.id] {
        assert_eq!(
            store
                .complete_session(
                    a.organization_id,
                    a.sandbox,
                    refused,
                    &completion(InstallationId::generate(), 1),
                    now,
                    &|_| by_suite_in(a.sandbox)
                )
                .await,
            Err(ConnectError::NotPending)
        );
        assert_eq!(
            store
                .find_session(a.organization_id, a.sandbox, refused)
                .await
                .unwrap()
                .unwrap()
                .installation_id,
            None
        );
    }
}

async fn renews_a_reconnects_installation_under_the_next_version(store: &dyn ConnectStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let now = Timestamp::now();
    let reconnect = |reference: &Reference| {
        let mut session = new(InstalledApp::Catalog(a.catalog_app), reference);
        session.intent = ConnectIntent::Reconnect;
        session.installation_id = Some(a.installation);
        session
    };
    let begin = || async {
        let reference = Reference::generate().unwrap();
        store
            .create_session(
                a.organization_id,
                a.sandbox,
                ConnectSessionId::generate(),
                &reconnect(&reference),
                now,
            )
            .await
            .unwrap()
    };

    let first = begin().await;
    for refused in [completion(InstallationId::generate(), 1), completion(a.installation, 2)] {
        assert_eq!(
            store
                .complete_session(a.organization_id, a.sandbox, first.id, &refused, now, &|_| by_suite_in(
                    a.sandbox
                ))
                .await,
            Err(ConnectError::InstallationChanged),
            "only its own installation, under the next version"
        );
    }
    assert_eq!(
        store
            .find_session(a.organization_id, a.sandbox, first.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        ConnectSessionStatus::Pending,
        "a refused completion writes nothing"
    );
    let (_, renewed) = store
        .complete_session(
            a.organization_id,
            a.sandbox,
            first.id,
            &completion(a.installation, 1),
            now,
            &|_| by_suite_in(a.sandbox),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            renewed.id,
            renewed.status,
            renewed.connection.map(|c| c.credential_version)
        ),
        (a.installation, InstallationStatus::Connected, Some(1)),
        "a disconnected installation that never held a credential starts at 1"
    );

    // Two reconnects read the same version; the second to complete loses.
    let (one, two) = (begin().await, begin().await);
    store
        .complete_session(
            a.organization_id,
            a.sandbox,
            one.id,
            &completion(a.installation, 2),
            now,
            &|_| by_suite_in(a.sandbox),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .complete_session(
                a.organization_id,
                a.sandbox,
                two.id,
                &completion(a.installation, 2),
                now,
                &|_| by_suite_in(a.sandbox)
            )
            .await,
        Err(ConnectError::InstallationChanged)
    );
    let (_, again) = store
        .complete_session(
            a.organization_id,
            a.sandbox,
            two.id,
            &completion(a.installation, 3),
            now,
            &|_| by_suite_in(a.sandbox),
        )
        .await
        .unwrap();
    assert_eq!(
        again.snapshot,
        Some(crate::installation_store::snapshot(3)),
        "a reconnect takes the template it reconnected under"
    );
    assert_eq!(again.connection.map(|c| c.credential_version), Some(3));
}
