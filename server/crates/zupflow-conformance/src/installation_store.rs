//! The `InstallationStore` suite (decisions 0031 and 0034): installations
//! reached only through their own workspace, of an app the workspace may
//! install, their credentials kept and returned exactly and gone once
//! disconnected or deleted, and the catalog-wide list paged without repeats;
//! reconnect required records its audit and outbox exactly once, only for
//! the current credential in its own scope. Each installation has a short ID
//! of its own in its workspace, kept while it is renamed or disconnected
//! (decision 0047). Disconnecting or deleting one queues, with it, the job
//! that refuses the calls waiting for approval on it, in its own workspace
//! and naming it alone (decision 0047).
//!
//! `prepare` makes a [`Tenant`] exist: its organization, two Sandbox and a
//! Production workspace, a catalog app, and an app of the Sandbox workspace's own.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use zupflow_core::apps::{AuthTemplate, OAuthScope};
use zupflow_core::audit::{
    AuditAction, AuditActor, AuditEntry, AuditFilter, AuditObject, AuditPlace, AuditStore, AuditTarget,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{
    ConnectedAccount, DiscoveredClient, Installation, InstallationError, InstallationStatus, InstallationStore,
    InstalledApp, NewCredential, NewInstallation, ShortId, TemplateSnapshot,
};
use zupflow_core::jobs::JobQueue;
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::runs::store::{REFUSE_WAITING_JOB, installation_of_job};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::webhooks::{DELIVER, NewEndpoint, WebhookEventType, WebhookStore, delivery_of};

use crate::audit_store::by_suite_in;

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    pub sandbox: WorkspaceId,
    /// Another workspace in the same environment, independent of its sibling.
    pub sibling: WorkspaceId,
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
        sibling: WorkspaceId::generate(),
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

/// The template an installation connected under: kept and returned exactly.
pub fn snapshot(app_version: u32) -> TemplateSnapshot {
    TemplateSnapshot {
        auth: AuthTemplate::OAuth2 {
            authorize_url: Some("https://accounts.example/authorize".to_owned()),
            token_url: Some("https://accounts.example/token".to_owned()),
            issuer: Some("https://ACCOUNTS.example/".to_owned()),
            available_scopes: vec![OAuthScope {
                name: "read".to_owned(),
                description: "Read".to_owned(),
            }],
            pkce: true,
            identity_url: None,
        },
        app_version,
        client: None,
    }
}

/// An `mcp_oauth` installation's template, with where it signed in: kept
/// and returned exactly too.
pub fn signed_in_snapshot(app_version: u32) -> TemplateSnapshot {
    TemplateSnapshot {
        auth: AuthTemplate::McpOAuth {
            scopes: Vec::new(),
            identity_url: None,
        },
        app_version,
        client: Some(DiscoveredClient {
            response_iss_supported: false,
            issuer: "https://auth.example/tenant".to_owned(),
            token_endpoint: "https://auth.example/tenant/token".to_owned(),
            client_id: "client-1".to_owned(),
            resource: "https://tools.example/mcp".to_owned(),
        }),
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
        snapshot: snapshot(u32::from(marker)),
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

/// `audit` and `jobs` read the effects written by the installation store.
/// The queue is isolated to this suite, with no workers consuming it.
pub async fn run(
    store: &dyn InstallationStore,
    audit: &dyn AuditStore,
    jobs: &dyn JobQueue,
    webhooks: &dyn WebhookStore,
    prepare: Prepare,
) {
    serializes_refresh_and_guards_every_scope(store, audit, jobs, webhooks, &prepare).await;
    creates_and_finds_within_its_workspace(store, &prepare).await;
    gives_each_installation_a_short_id_of_its_own(store, &prepare).await;
    refuses_an_app_the_workspace_cannot_install(store, &prepare).await;
    lists_its_own_workspace_newest_first(store, &prepare).await;
    changes_only_within_its_workspace(store, &prepare).await;
    forgets_the_credential_when_disconnected_or_deleted(store, &prepare).await;
    lists_a_catalog_apps_installations_page_by_page(store, &prepare).await;
    counts_installations_per_app(store, &prepare).await;
    queues_the_refusals_when_it_goes(store, jobs, &prepare).await;
}

/// The jobs refusing waiting calls queued so far, each with its scope and
/// the installation it names, claimed and completed.
async fn refusals(jobs: &dyn JobQueue) -> Vec<(Scope, InstallationId)> {
    let kind = Name::new(REFUSE_WAITING_JOB).unwrap();
    let mut queued = Vec::new();
    while let Some(job) = jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
    {
        jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
        queued.push((
            job.scope,
            installation_of_job(&job.payload).expect("names an installation"),
        ));
    }
    queued
}

async fn queues_the_refusals_when_it_goes(store: &dyn InstallationStore, jobs: &dyn JobQueue, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    refusals(jobs).await;
    let here = install(store, &a, a.sandbox, InstalledApp::Catalog(a.catalog_app)).await;
    let sibling = install(store, &a, a.sibling, InstalledApp::Catalog(a.catalog_app)).await;
    let theirs = install(store, &b, b.sandbox, InstalledApp::Catalog(b.catalog_app)).await;
    assert!(refusals(jobs).await.is_empty(), "nothing queued by connecting");

    // Refused from any other scope: nothing queued.
    for (organization, workspace, installation) in [
        (b.organization_id, a.sandbox, here.id),
        (a.organization_id, a.sibling, here.id),
        (a.organization_id, a.sandbox, theirs.id),
    ] {
        assert_eq!(
            store
                .disconnect_installation(organization, workspace, installation, &|_| by_suite_in(workspace))
                .await,
            Err(InstallationError::NotFound)
        );
        assert_eq!(
            store
                .delete_installation(organization, workspace, installation, &|_| by_suite_in(workspace))
                .await,
            Err(InstallationError::NotFound)
        );
    }
    assert!(refusals(jobs).await.is_empty(), "nothing queued by a refused change");

    store
        .disconnect_installation(a.organization_id, a.sandbox, here.id, &|_| by_suite_in(a.sandbox))
        .await
        .unwrap();
    assert_eq!(
        refusals(jobs).await,
        [(Scope::Workspace(a.organization_id, a.sandbox), here.id)],
        "one job, in its workspace, naming it"
    );
    store
        .disconnect_installation(a.organization_id, a.sandbox, here.id, &|_| by_suite_in(a.sandbox))
        .await
        .unwrap();
    assert!(refusals(jobs).await.is_empty(), "a repeated disconnect queues nothing");
    store
        .delete_installation(a.organization_id, a.sandbox, here.id, &|_| by_suite_in(a.sandbox))
        .await
        .unwrap();
    store
        .delete_installation(a.organization_id, a.sibling, sibling.id, &|_| by_suite_in(a.sibling))
        .await
        .unwrap();
    assert_eq!(
        refusals(jobs).await,
        [
            (Scope::Workspace(a.organization_id, a.sandbox), here.id),
            (Scope::Workspace(a.organization_id, a.sibling), sibling.id),
        ],
        "deleting queues one too, each in its own workspace"
    );
    assert!(
        store
            .find_installation(b.organization_id, b.sandbox, theirs.id)
            .await
            .unwrap()
            .is_some_and(|installation| installation.status == InstallationStatus::Connected),
        "the other organization's untouched"
    );
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
    assert_eq!(created.snapshot, Some(snapshot(7)), "the template it connected under");
    let signed_in = store
        .create_installation(
            a.organization_id,
            a.sandbox,
            InstallationId::generate(),
            &NewInstallation {
                snapshot: signed_in_snapshot(2),
                ..new(InstalledApp::Catalog(a.catalog_app), "Signed in", 8)
            },
        )
        .await
        .unwrap();
    assert_eq!(signed_in.snapshot, Some(signed_in_snapshot(2)), "where it signed in");
    assert_eq!(
        store
            .find_installation(a.organization_id, a.sandbox, signed_in.id)
            .await
            .unwrap()
            .and_then(|found| found.snapshot),
        Some(signed_in_snapshot(2))
    );
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

async fn gives_each_installation_a_short_id_of_its_own(store: &dyn InstallationStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let mut issued: Vec<(WorkspaceId, ShortId)> = Vec::new();
    for (tenant, workspace) in [(a, a.sandbox), (a, a.sibling), (b, b.sandbox)] {
        let mut in_workspace = HashSet::new();
        for _ in 0..12 {
            let installation = install(store, &tenant, workspace, InstalledApp::Catalog(tenant.catalog_app)).await;
            assert_eq!(
                ShortId::parse(installation.short_id.as_str()),
                Some(installation.short_id.clone()),
                "six lowercase base32 characters"
            );
            assert!(
                in_workspace.insert(installation.short_id.clone()),
                "unique in its workspace"
            );
            issued.push((workspace, installation.short_id));
        }
        let listed: HashSet<ShortId> = store
            .list_installations(tenant.organization_id, workspace)
            .await
            .unwrap()
            .into_iter()
            .map(|installation| installation.short_id)
            .collect();
        assert_eq!(listed, in_workspace, "read back as given");
    }

    // Kept while it is renamed and disconnected, and not issued again once
    // it is deleted.
    let installation = install(store, &a, a.sandbox, InstalledApp::Workspace(a.own_app)).await;
    let renamed = store
        .rename_installation(a.organization_id, a.sandbox, installation.id, "Renamed", &|_| {
            by_suite_in(a.sandbox)
        })
        .await
        .unwrap();
    assert_eq!(renamed.short_id, installation.short_id);
    let disconnected = store
        .disconnect_installation(a.organization_id, a.sandbox, installation.id, &|_| {
            by_suite_in(a.sandbox)
        })
        .await
        .unwrap();
    assert_eq!(disconnected.short_id, installation.short_id);
    store
        .delete_installation(a.organization_id, a.sandbox, installation.id, &|_| {
            by_suite_in(a.sandbox)
        })
        .await
        .unwrap();
    for _ in 0..12 {
        let next = install(store, &a, a.sandbox, InstalledApp::Catalog(a.catalog_app)).await;
        assert_ne!(
            next.short_id, installation.short_id,
            "a deleted installation's short ID"
        );
        assert!(
            !issued.contains(&(a.sandbox, next.short_id.clone())),
            "issued once in its workspace"
        );
        issued.push((a.sandbox, next.short_id));
    }
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
                .rename_installation(organization, workspace, installation.id, "Stolen", &|_| by_suite_in(
                    workspace
                ))
                .await,
            Err(InstallationError::NotFound)
        );
        assert_eq!(
            store
                .disconnect_installation(organization, workspace, installation.id, &|_| by_suite_in(workspace))
                .await,
            Err(InstallationError::NotFound)
        );
        assert_eq!(
            store
                .delete_installation(organization, workspace, installation.id, &|_| by_suite_in(workspace))
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
        .rename_installation(a.organization_id, a.sandbox, installation.id, "Renamed", &|_| {
            by_suite_in(a.sandbox)
        })
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
        .disconnect_installation(a.organization_id, a.sandbox, disconnected.id, &|_| {
            by_suite_in(a.sandbox)
        })
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
        .delete_installation(a.organization_id, a.production, deleted.id, &|_| {
            by_suite_in(a.production)
        })
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

async fn serializes_refresh_and_guards_every_scope(
    store: &dyn InstallationStore,
    audit_store: &dyn AuditStore,
    jobs: &dyn JobQueue,
    webhooks: &dyn WebhookStore,
    prepare: &Prepare,
) {
    use zupflow_core::{installations::RefreshClaim, time::Timestamp};
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let i = install(store, &a, a.sandbox, InstalledApp::Catalog(a.catalog_app)).await;
    let now = Timestamp::from_millis(1000);
    let mut next = credential(2);
    next.version = 2;
    let audit = |i: &Installation| {
        AuditEntry::new(
            AuditActor::System {
                name: "Credentials".to_owned(),
            },
            AuditAction::InstallationReconnectRequired,
            AuditTarget::new(AuditObject::Installation(i.id), &i.label),
            AuditPlace::Workspace(a.sandbox),
        )
    };
    webhooks
        .create_endpoint(
            a.organization_id,
            zupflow_core::ids::WebhookEndpointId::generate(),
            &NewEndpoint {
                environment: Environment::Sandbox,
                url: "https://hooks.example/refresh".to_owned(),
                description: String::new(),
                events: vec![WebhookEventType::InstallationReconnectRequired],
                secret: credential(1).sealed,
                secret_last4: "last".to_owned(),
            },
            now,
            &crate::audit_store::by_suite,
        )
        .await
        .unwrap();
    let events = |org| async move {
        audit_store
            .list_audit_events(
                org,
                &AuditFilter {
                    workspace_id: Some(a.sandbox),
                },
                None,
                100,
            )
            .await
            .unwrap()
    };
    let kind = Name::new(DELIVER).unwrap();
    let announced = || async {
        let mut claimed = Vec::new();
        while let Some(job) = jobs
            .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
            .await
            .unwrap()
        {
            jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
            claimed.push((
                job.scope,
                serde_json::from_str::<serde_json::Value>(
                    &webhooks
                        .find_delivery(a.organization_id, delivery_of(&job.payload).unwrap())
                        .await
                        .unwrap()
                        .unwrap()
                        .body,
                )
                .unwrap(),
            ));
        }
        claimed
    };
    assert!(announced().await.is_empty());
    for (org, ws) in [
        (b.organization_id, a.sandbox),
        (a.organization_id, a.sibling),
        (a.organization_id, a.production),
        (b.organization_id, b.sandbox),
    ] {
        assert_eq!(
            store.claim_refresh(org, ws, i.id, 1, now).await.unwrap(),
            RefreshClaim::Changed
        );
        assert!(
            !store
                .rotate_credential(org, ws, i.id, &next, Some(now), now)
                .await
                .unwrap()
        );
        assert!(!store.require_reconnect(org, ws, i.id, 1, now, &audit).await.unwrap());
        assert_eq!(events(a.organization_id).await.total, 0, "wrong scope records no audit");
        assert_eq!(events(b.organization_id).await.total, 0);
        assert!(announced().await.is_empty(), "wrong scope announces nothing");
    }
    assert!(
        !store
            .rotate_credential(a.organization_id, a.sandbox, i.id, &next, None, now)
            .await
            .unwrap(),
        "must claim before rotating"
    );
    let (one, two) = tokio::join!(
        store.claim_refresh(a.organization_id, a.sandbox, i.id, 1, now),
        store.claim_refresh(a.organization_id, a.sandbox, i.id, 1, now)
    );
    assert!(matches!(
        (one.unwrap(), two.unwrap()),
        (RefreshClaim::Claimed, RefreshClaim::Pending { .. }) | (RefreshClaim::Pending { .. }, RefreshClaim::Claimed)
    ));
    assert_eq!(
        store
            .claim_refresh(a.organization_id, a.sandbox, i.id, 1, Timestamp::from_millis(999999))
            .await
            .unwrap(),
        RefreshClaim::Pending { started_at: now },
        "an abandoned exchange can never be claimed again"
    );
    assert!(
        store
            .rotate_credential(a.organization_id, a.sandbox, i.id, &next, Some(now), now)
            .await
            .unwrap()
    );
    assert_eq!(
        store.find_credential(a.organization_id, a.sandbox, i.id).await.unwrap(),
        Some(next.clone())
    );
    let rotated = store
        .find_installation(a.organization_id, a.sandbox, i.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rotated.connection.unwrap().expires_at, Some(now));
    assert!(
        !store
            .rotate_credential(a.organization_id, a.sandbox, i.id, &next, None, now)
            .await
            .unwrap()
    );
    assert!(
        !store
            .require_reconnect(a.organization_id, a.sandbox, i.id, 1, now, &audit)
            .await
            .unwrap(),
        "stale failure cannot flag winner"
    );
    assert_eq!(
        events(a.organization_id).await.total,
        0,
        "stale failure records no audit"
    );
    assert!(announced().await.is_empty(), "stale failure announces nothing");
    assert_eq!(
        store
            .require_reconnect(a.organization_id, a.sandbox, i.id, 2, now, &|i| AuditEntry {
                place: AuditPlace::Workspace(a.sibling),
                ..audit(i)
            })
            .await,
        Err(InstallationError::Unavailable),
        "an audit placed in another workspace fails the whole transition"
    );
    assert_eq!(
        store
            .find_installation(a.organization_id, a.sandbox, i.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        InstallationStatus::Connected
    );
    assert_eq!(events(a.organization_id).await.total, 0, "failed audit rolls back");
    assert!(announced().await.is_empty(), "failed audit queues no announcement");
    let (one, two) = tokio::join!(
        store.require_reconnect(a.organization_id, a.sandbox, i.id, 2, now, &audit),
        store.require_reconnect(a.organization_id, a.sandbox, i.id, 2, now, &audit)
    );
    assert_ne!(one.unwrap(), two.unwrap(), "only one worker commits the transition");
    assert!(
        !store
            .require_reconnect(a.organization_id, a.sandbox, i.id, 2, now, &audit)
            .await
            .unwrap(),
        "exactly one transition"
    );
    let page = events(a.organization_id).await;
    assert_eq!(
        page.total, 1,
        "one audit event, including concurrent and repeated attempts"
    );
    assert_eq!(page.items.len(), 1);
    let event = &page.items[0];
    let expected = audit(&i);
    assert_eq!(event.organization_id, a.organization_id);
    assert_eq!(event.workspace_id, Some(a.sandbox));
    assert_eq!(event.workspace.as_ref().map(|workspace| workspace.id), Some(a.sandbox));
    assert_eq!(event.environment, Some(Environment::Sandbox));
    assert_eq!(event.actor, expected.actor);
    assert_eq!(event.action, expected.action);
    assert_eq!(event.target, expected.target);
    assert_eq!(events(b.organization_id).await.total, 0);
    let claimed = announced().await;
    assert_eq!(claimed.len(), 1, "one outbox event for the committed transition");
    let (scope, announcement) = &claimed[0];
    assert_eq!(*scope, Scope::Organization(a.organization_id));
    assert_eq!(announcement["type"], "installation.reconnect_required");
    assert_eq!(announcement["created_at"], serde_json::json!(now));
    assert_eq!(
        announcement["data"],
        serde_json::json!({ "object": "installation", "id": i.id,
        "app_id": a.catalog_app, "status": "reconnect_required" })
    );
    assert_eq!(
        store
            .find_installation(a.organization_id, a.sandbox, i.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        InstallationStatus::ReconnectRequired
    );
    assert_eq!(
        store
            .claim_refresh(a.organization_id, a.sandbox, i.id, 2, now)
            .await
            .unwrap(),
        RefreshClaim::Changed
    );
    let other = install(store, &a, a.sandbox, InstalledApp::Catalog(a.catalog_app)).await;
    store
        .claim_refresh(a.organization_id, a.sandbox, other.id, 1, now)
        .await
        .unwrap();
    store
        .disconnect_installation(a.organization_id, a.sandbox, other.id, &|_| by_suite_in(a.sandbox))
        .await
        .unwrap();
    assert!(
        !store
            .rotate_credential(a.organization_id, a.sandbox, other.id, &next, None, now)
            .await
            .unwrap(),
        "disconnect stops a late refresh"
    );
    let before = events(a.organization_id).await;
    assert!(
        !store
            .require_reconnect(a.organization_id, a.sandbox, other.id, 1, now, &audit)
            .await
            .unwrap()
    );
    assert_eq!(
        events(a.organization_id).await,
        before,
        "disconnected credentials record no reconnect audit"
    );
    assert!(
        announced().await.is_empty(),
        "disconnected credentials announce nothing"
    );
}
