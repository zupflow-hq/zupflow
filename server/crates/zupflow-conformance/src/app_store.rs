//! The `AppStore` suite (decision 0031): catalog apps reached only through
//! their organization, workspace apps only through their own workspace, never
//! one through the other's methods, and slugs unique within their owner.
//!
//! `prepare` makes an organization exist with the given workspaces in it, for
//! a store whose apps reference them.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::apps::{
    App, AppChanges, AppError, AppStore, AuthTemplate, AuthType, AuthorizationServer, ClientAuthentication,
    ClientRegistration, ClientRegistrationId, ConfigurationChanges, NewApp, NewClientRegistration, NewClientSecret,
    StoredClientSecret,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, OrganizationId, WorkspaceId};
use zupflow_core::keys::{Sealed, WrappedKey};

use crate::audit_store::{by_suite, by_suite_in};

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
        .create_organization_app(org, AppId::generate(), &app, &by_suite)
        .await
        .unwrap()
}

async fn workspace_app(store: &dyn AppStore, org: OrganizationId, workspace: WorkspaceId, app: NewApp) -> App {
    store
        .create_workspace_app(org, workspace, AppId::generate(), &app, &|_| by_suite_in(workspace))
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
    saves_configurations_field_by_field(store, &prepare).await;
    keeps_configurations_to_their_app_and_environment(store, &prepare).await;
    takes_each_secret_version_once(store, &prepare).await;
    keeps_requested_scopes_offered(store, &prepare).await;
    keeps_one_client_registration_per_owner_and_issuer(store, &prepare).await;
    keeps_the_first_of_concurrent_registrations(store, &prepare).await;
    keeps_workspace_registrations_to_their_workspace(store, &prepare).await;
    forgets_only_the_failed_registration(store, &prepare).await;
}

fn server(issuer: &str) -> AuthorizationServer {
    AuthorizationServer {
        response_iss_supported: false,
        issuer: issuer.to_owned(),
        authorization_endpoint: format!("{issuer}/authorize"),
        token_endpoint: format!("{issuer}/token"),
        registration_endpoint: format!("{issuer}/register"),
    }
}

/// A registration at `issuer`; with a marker, a client that was issued a secret.
fn registration(issuer: &str, client_id: &str, marker: Option<u8>) -> NewClientRegistration {
    NewClientRegistration {
        id: ClientRegistrationId::generate(),
        server: server(issuer),
        client_id: client_id.to_owned(),
        authentication: if marker.is_some() {
            ClientAuthentication::ClientSecretBasic
        } else {
            ClientAuthentication::None
        },
        sealed: marker.map(|marker| Sealed {
            wrapped_key: WrappedKey {
                key_id: "local:1".to_owned(),
                bytes: vec![marker; 40],
            },
            nonce: [marker; 12],
            ciphertext: vec![marker; 24],
        }),
    }
}

/// What the store keeps of `new`.
fn kept(new: &NewClientRegistration, found: &ClientRegistration) {
    assert_eq!(
        (
            found.id,
            &found.server,
            found.client_id.as_str(),
            found.authentication,
            &found.sealed
        ),
        (
            new.id,
            &new.server,
            new.client_id.as_str(),
            new.authentication,
            &new.sealed
        )
    );
}

const ISSUER: &str = "https://auth.example/tenant";

/// One client per catalog app, environment and issuer: a second at the same
/// issuer keeps the first, another issuer is another registration, and
/// nothing reaches another app, environment or organization.
async fn keeps_one_client_registration_per_owner_and_issuer(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, _) = organization(prepare).await;
    let (b, _, _) = organization(prepare).await;
    let sign_in =
        |name: &str, slug: &str| NewApp::blank(name.to_owned(), slug.to_owned(), String::new(), AuthType::McpOAuth);
    let app = catalog_app(store, a, sign_in("Ledger", "ledger")).await;
    let sibling = catalog_app(store, a, sign_in("Desk", "desk")).await;
    let own = workspace_app(store, a, one, sign_in("Ledger", "ledger")).await;
    let sandbox = Environment::Sandbox;

    assert_eq!(
        store
            .find_organization_client_registration(a, app.id, sandbox, ISSUER)
            .await,
        Ok(None)
    );
    let first = registration(ISSUER, "client-1", Some(1));
    let saved = store
        .save_organization_client_registration(a, app.id, sandbox, &first)
        .await
        .unwrap();
    kept(&first, &saved);
    let found = store
        .find_organization_client_registration(a, app.id, sandbox, ISSUER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found, saved);

    // A second at the same issuer keeps the first, unchanged.
    let second = registration(ISSUER, "client-2", None);
    assert_eq!(
        store
            .save_organization_client_registration(a, app.id, sandbox, &second)
            .await,
        Ok(saved.clone())
    );

    // Another issuer is another registration; the first stays.
    let moved = registration("https://auth.example/moved", "client-3", None);
    kept(
        &moved,
        &store
            .save_organization_client_registration(a, app.id, sandbox, &moved)
            .await
            .unwrap(),
    );
    assert_eq!(
        store
            .find_organization_client_registration(a, app.id, sandbox, ISSUER)
            .await,
        Ok(Some(saved.clone()))
    );

    // Not another environment's, another app's, or a workspace app's.
    for (app_id, environment) in [
        (app.id, Environment::Production),
        (sibling.id, sandbox),
        (own.id, sandbox),
    ] {
        assert_eq!(
            store
                .find_organization_client_registration(a, app_id, environment, ISSUER)
                .await,
            Ok(None)
        );
    }
    let production = registration(ISSUER, "client-4", None);
    kept(
        &production,
        &store
            .save_organization_client_registration(a, app.id, Environment::Production, &production)
            .await
            .unwrap(),
    );

    // Another organization reaches nothing, and keeps nothing for an app not its own.
    assert_eq!(
        store
            .find_organization_client_registration(b, app.id, sandbox, ISSUER)
            .await,
        Ok(None)
    );
    let new = registration(ISSUER, "client-5", None);
    assert_eq!(
        store
            .save_organization_client_registration(b, app.id, sandbox, &new)
            .await,
        Err(AppError::NotFound)
    );
    // The catalog's methods don't take a workspace app.
    assert_eq!(
        store
            .save_organization_client_registration(a, own.id, sandbox, &new)
            .await,
        Err(AppError::NotFound)
    );
}

/// Concurrent first registrations keep exactly one, and both are told which.
async fn keeps_the_first_of_concurrent_registrations(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, _) = organization(prepare).await;
    let app = catalog_app(
        store,
        a,
        NewApp::blank("Ledger".into(), "ledger".into(), String::new(), AuthType::McpOAuth),
    )
    .await;
    let own = workspace_app(
        store,
        a,
        one,
        NewApp::blank("Ledger".into(), "ledger".into(), String::new(), AuthType::McpOAuth),
    )
    .await;
    let (left, right) = (
        registration(ISSUER, "left", Some(2)),
        registration(ISSUER, "right", None),
    );
    let (l, r) = tokio::join!(
        store.save_organization_client_registration(a, app.id, Environment::Sandbox, &left),
        store.save_organization_client_registration(a, app.id, Environment::Sandbox, &right),
    );
    let (l, r) = (l.unwrap(), r.unwrap());
    assert_eq!(l, r, "both use the one kept");
    assert!(l.id == left.id || l.id == right.id);
    let (l, r) = tokio::join!(
        store.save_workspace_client_registration(a, one, own.id, &left),
        store.save_workspace_client_registration(a, one, own.id, &right),
    );
    let (l, r) = (l.unwrap(), r.unwrap());
    assert_eq!(l, r, "both use the one kept");
}

/// A workspace app's client belongs to its workspace alone.
async fn keeps_workspace_registrations_to_their_workspace(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, two) = organization(prepare).await;
    let (b, other, _) = organization(prepare).await;
    let sign_in = || NewApp::blank("Ledger".into(), "ledger".into(), String::new(), AuthType::McpOAuth);
    let own = workspace_app(store, a, one, sign_in()).await;
    let neighbour = workspace_app(store, a, two, sign_in()).await;
    let catalog = catalog_app(store, a, sign_in()).await;

    let first = registration(ISSUER, "client-1", None);
    let saved = store
        .save_workspace_client_registration(a, one, own.id, &first)
        .await
        .unwrap();
    kept(&first, &saved);
    assert_eq!(
        store.find_workspace_client_registration(a, one, own.id, ISSUER).await,
        Ok(Some(saved))
    );
    // Not through another workspace, another organization, or the catalog.
    for (organization_id, workspace_id, app_id) in [(a, two, own.id), (b, other, own.id), (a, one, neighbour.id)] {
        assert_eq!(
            store
                .find_workspace_client_registration(organization_id, workspace_id, app_id, ISSUER)
                .await,
            Ok(None)
        );
    }
    assert_eq!(
        store
            .find_organization_client_registration(a, own.id, Environment::Sandbox, ISSUER)
            .await,
        Ok(None)
    );
    // Nothing kept for another workspace's app, or a catalog app.
    for (organization_id, workspace_id, app_id) in [(a, two, own.id), (b, other, own.id), (a, one, catalog.id)] {
        assert_eq!(
            store
                .save_workspace_client_registration(
                    organization_id,
                    workspace_id,
                    app_id,
                    &registration(ISSUER, "client-2", None)
                )
                .await,
            Err(AppError::NotFound)
        );
    }
}

fn oauth_template(scopes: &[&str]) -> AuthTemplate {
    AuthTemplate::OAuth2 {
        authorize_url: None,
        token_url: None,
        available_scopes: scopes
            .iter()
            .map(|name| zupflow_core::apps::OAuthScope {
                name: (*name).to_owned(),
                description: String::new(),
            })
            .collect(),
        pkce: true,
        identity_url: None,
    }
}

fn with(template: &AuthTemplate) -> AppChanges<'_> {
    AppChanges {
        auth: Some(template),
        ..AppChanges::default()
    }
}

fn requesting(scopes: &[&str]) -> ConfigurationChanges {
    ConfigurationChanges {
        oauth_scopes: Some(scopes.iter().map(|scope| (*scope).to_owned()).collect()),
        ..ConfigurationChanges::default()
    }
}

/// A configuration never requests a scope its app doesn't offer, whichever
/// write comes first; and only the app's own configurations count.
async fn keeps_requested_scopes_offered(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, two) = organization(prepare).await;
    let oauth =
        |name: &str, slug: &str| NewApp::blank(name.to_owned(), slug.to_owned(), String::new(), AuthType::OAuth2);
    let catalog = catalog_app(store, a, oauth("Tracker", "tracker")).await;
    let sibling = catalog_app(store, a, oauth("Other", "other")).await;
    let own = workspace_app(store, a, one, oauth("Crm", "crm")).await;
    let neighbour = workspace_app(store, a, two, oauth("Crm", "crm")).await;
    let beside = workspace_app(store, a, one, oauth("Desk", "desk")).await;
    let read = oauth_template(&["read"]);
    for app in [catalog.id, sibling.id] {
        store
            .update_organization_app(a, app, with(&read), &by_suite)
            .await
            .unwrap();
    }
    for (workspace, app) in [(one, own.id), (two, neighbour.id), (one, beside.id)] {
        store
            .update_workspace_app(a, workspace, app, with(&read), &|_| by_suite_in(workspace))
            .await
            .unwrap();
    }

    // A scope the template doesn't offer can't be requested.
    assert_eq!(
        store
            .save_organization_configuration(
                a,
                catalog.id,
                Environment::Production,
                &requesting(&["write"]),
                &by_suite
            )
            .await,
        Err(AppError::UnknownScope("write".to_owned()))
    );
    assert_eq!(
        store
            .save_workspace_configuration(a, one, own.id, &requesting(&["write"]), &|_| by_suite_in(one))
            .await,
        Err(AppError::UnknownScope("write".to_owned()))
    );

    // Others requesting `read` don't hold this app's template back...
    store
        .save_organization_configuration(a, sibling.id, Environment::Sandbox, &requesting(&["read"]), &by_suite)
        .await
        .unwrap();
    store
        .save_workspace_configuration(a, two, neighbour.id, &requesting(&["read"]), &|_| by_suite_in(two))
        .await
        .unwrap();
    // An app beside it in the same workspace doesn't either.
    store
        .save_workspace_configuration(a, one, beside.id, &requesting(&["read"]), &|_| by_suite_in(one))
        .await
        .unwrap();
    let none = oauth_template(&[]);
    store
        .update_organization_app(a, catalog.id, with(&none), &by_suite)
        .await
        .unwrap();
    store
        .update_workspace_app(a, one, own.id, with(&none), &|_| by_suite_in(one))
        .await
        .unwrap();

    // ...but its own configuration requesting it does.
    assert_eq!(
        store
            .update_organization_app(a, sibling.id, with(&none), &by_suite)
            .await,
        Err(AppError::ScopeInUse {
            environment: Environment::Sandbox,
            scope: "read".to_owned(),
        })
    );
    assert_eq!(
        store
            .update_workspace_app(a, two, neighbour.id, with(&none), &|_| by_suite_in(two))
            .await,
        Err(AppError::ScopeInUse {
            environment: Environment::Sandbox,
            scope: "read".to_owned(),
        })
    );
    assert_eq!(
        store
            .find_organization_app(a, sibling.id)
            .await
            .unwrap()
            .map(|app| app.auth),
        Some(read),
        "a refused template writes nothing"
    );
}

/// Opaque to the store: it keeps and returns these bytes exactly.
fn new_secret(version: u32, marker: u8) -> NewClientSecret {
    NewClientSecret {
        sealed: Sealed {
            wrapped_key: WrappedKey {
                key_id: format!("key-{marker}"),
                bytes: vec![marker; 40],
            },
            nonce: [marker; 12],
            ciphertext: vec![marker; 24],
        },
        last4: format!("{marker:04}"),
        version,
    }
}

fn set_url(url: &str) -> ConfigurationChanges {
    ConfigurationChanges {
        mcp_server_url: Some(Some(url.to_owned())),
        ..ConfigurationChanges::default()
    }
}

async fn saves_configurations_field_by_field(store: &dyn AppStore, prepare: &Prepare) {
    let (org, one, _) = organization(prepare).await;
    let app = catalog_app(store, org, new("Linear", "linear")).await;
    let sandbox = Environment::Sandbox;
    assert_eq!(
        store
            .find_organization_configuration(org, app.id, sandbox)
            .await
            .unwrap(),
        None
    );

    let first = store
        .save_organization_configuration(
            org,
            app.id,
            sandbox,
            &set_url("https://mcp.linear.example/mcp"),
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!((first.app_id, first.environment, first.version), (app.id, sandbox, 1));
    assert_eq!(first.mcp_server_url.as_deref(), Some("https://mcp.linear.example/mcp"));
    assert_eq!(
        (first.oauth_client_id.as_ref(), first.client_secret.as_ref()),
        (None, None)
    );
    assert!(first.updated_at.is_some());

    // Only what a write names changes.
    let second = store
        .save_organization_configuration(
            org,
            app.id,
            sandbox,
            &ConfigurationChanges {
                oauth_client_id: Some(Some("client".to_owned())),
                oauth_scopes: Some(vec!["read".to_owned()]),
                client_secret: Some(new_secret(1, 7)),
                ..ConfigurationChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!(second.version, 2);
    assert_eq!(second.mcp_server_url, first.mcp_server_url, "kept");
    assert_eq!(second.oauth_client_id.as_deref(), Some("client"));
    assert_eq!(second.oauth_scopes, ["read"]);
    let state = second.client_secret.clone().unwrap();
    assert_eq!((state.last4.as_str(), state.version), ("0007", 1));
    assert_eq!(
        store
            .find_organization_configuration(org, app.id, sandbox)
            .await
            .unwrap(),
        Some(second.clone())
    );

    // `Some(None)` clears; the secret stays.
    let cleared = store
        .save_organization_configuration(
            org,
            app.id,
            sandbox,
            &ConfigurationChanges {
                mcp_server_url: Some(None),
                ..ConfigurationChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!((cleared.version, cleared.mcp_server_url.as_ref()), (3, None));
    assert_eq!(cleared.client_secret, second.client_secret);

    // A workspace app's, in its workspace's environment.
    let own = workspace_app(store, org, one, new("Echo", "echo")).await;
    assert_eq!(
        store.find_workspace_configuration(org, one, own.id).await.unwrap(),
        None
    );
    let saved = store
        .save_workspace_configuration(org, one, own.id, &set_url("https://echo.example/mcp"), &|_| {
            by_suite_in(one)
        })
        .await
        .unwrap();
    assert_eq!(
        (saved.app_id, saved.environment, saved.version),
        (own.id, Environment::Sandbox, 1)
    );
    assert_eq!(
        store.find_workspace_configuration(org, one, own.id).await.unwrap(),
        Some(saved)
    );
}

async fn keeps_configurations_to_their_app_and_environment(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, two) = organization(prepare).await;
    let (b, b_workspace, _) = organization(prepare).await;
    let catalog = catalog_app(store, a, new("Linear", "linear")).await;
    let own = workspace_app(store, a, one, new("Echo", "echo")).await;
    let theirs = catalog_app(store, b, new("Linear", "linear")).await;
    let changes = set_url("https://mcp.example/mcp");
    store
        .save_organization_configuration(a, catalog.id, Environment::Sandbox, &changes, &by_suite)
        .await
        .unwrap();
    store
        .save_workspace_configuration(a, one, own.id, &changes, &|_| by_suite_in(one))
        .await
        .unwrap();

    // The environments are separate.
    assert_eq!(
        store
            .find_organization_configuration(a, catalog.id, Environment::Production)
            .await
            .unwrap(),
        None
    );
    // Through the wrong owner, nothing is found or written.
    assert_eq!(
        store
            .find_organization_configuration(b, catalog.id, Environment::Sandbox)
            .await
            .unwrap(),
        None
    );
    for (org, workspace) in [(a, two), (b, one), (b, b_workspace)] {
        assert_eq!(
            store
                .find_workspace_configuration(org, workspace, own.id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .save_workspace_configuration(org, workspace, own.id, &changes, &|_| by_suite_in(workspace))
                .await,
            Err(AppError::NotFound)
        );
    }
    for (org, app) in [(b, catalog.id), (a, own.id), (a, theirs.id)] {
        assert_eq!(
            store
                .save_organization_configuration(org, app, Environment::Sandbox, &changes, &by_suite)
                .await,
            Err(AppError::NotFound)
        );
    }
    assert_eq!(
        store
            .save_workspace_configuration(a, one, catalog.id, &changes, &|_| by_suite_in(one))
            .await,
        Err(AppError::NotFound),
        "a catalog app isn't a workspace's"
    );
    let listed = store.list_organization_configurations(a).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].app_id, listed[0].environment),
        (catalog.id, Environment::Sandbox)
    );
    assert_eq!(store.list_organization_configurations(b).await.unwrap(), []);
    let own_listed = store.list_workspace_configurations(a, one).await.unwrap();
    assert_eq!(own_listed.len(), 1);
    assert_eq!(own_listed[0].app_id, own.id);
    assert_eq!(store.list_workspace_configurations(a, two).await.unwrap(), []);
    assert_eq!(store.list_workspace_configurations(b, one).await.unwrap(), []);
}

async fn takes_each_secret_version_once(store: &dyn AppStore, prepare: &Prepare) {
    let (a, one, two) = organization(prepare).await;
    let (b, _, _) = organization(prepare).await;
    let catalog = catalog_app(store, a, new("Linear", "linear")).await;
    let own = workspace_app(store, a, one, new("Echo", "echo")).await;
    let with = |secret| ConfigurationChanges {
        client_secret: Some(secret),
        ..ConfigurationChanges::default()
    };
    let env = Environment::Production;

    // The first secret is version 1, and each next one the stored one plus one.
    for wrong in [0, 2] {
        assert_eq!(
            store
                .save_organization_configuration(a, catalog.id, env, &with(new_secret(wrong, 1)), &by_suite)
                .await,
            Err(AppError::SecretVersionTaken),
            "version {wrong} first"
        );
    }
    assert_eq!(
        store.find_organization_configuration(a, catalog.id, env).await.unwrap(),
        None,
        "a refused write writes nothing"
    );
    store
        .save_organization_configuration(a, catalog.id, env, &with(new_secret(1, 1)), &by_suite)
        .await
        .unwrap();
    assert_eq!(
        store
            .save_organization_configuration(a, catalog.id, env, &with(new_secret(1, 2)), &by_suite)
            .await,
        Err(AppError::SecretVersionTaken),
        "version 1 is taken"
    );
    let saved = store
        .save_organization_configuration(a, catalog.id, env, &with(new_secret(2, 3)), &by_suite)
        .await
        .unwrap();
    assert_eq!(saved.client_secret.unwrap().version, 2);
    let expected = new_secret(2, 3);
    assert_eq!(
        store.find_organization_client_secret(a, catalog.id, env).await.unwrap(),
        Some(StoredClientSecret {
            sealed: expected.sealed,
            version: 2,
        }),
        "the bytes come back exactly"
    );
    assert_eq!(
        store.find_organization_client_secret(b, catalog.id, env).await.unwrap(),
        None
    );
    assert_eq!(
        store
            .find_organization_client_secret(a, catalog.id, Environment::Sandbox)
            .await
            .unwrap(),
        None
    );

    store
        .save_workspace_configuration(a, one, own.id, &with(new_secret(1, 4)), &|_| by_suite_in(one))
        .await
        .unwrap();
    assert_eq!(
        store
            .save_workspace_configuration(a, one, own.id, &with(new_secret(1, 5)), &|_| by_suite_in(one))
            .await,
        Err(AppError::SecretVersionTaken)
    );
    let stored = store
        .find_workspace_client_secret(a, one, own.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((stored.version, stored.sealed.nonce), (1, [4; 12]));
    assert_eq!(store.find_workspace_client_secret(a, two, own.id).await.unwrap(), None);
    assert_eq!(store.find_workspace_client_secret(b, one, own.id).await.unwrap(), None);
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
            .create_organization_app(a, AppId::generate(), &new("Other", "tools"), &by_suite)
            .await,
        Err(AppError::SlugTaken)
    );
    assert_eq!(
        store
            .create_workspace_app(a, one, AppId::generate(), &new("Other", "tools"), &|_| by_suite_in(one))
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
        ..AppChanges::default()
    };

    // Through the wrong owner, nothing changes.
    for result in [
        store.update_organization_app(b, catalog.id, rename, &by_suite).await,
        store.update_organization_app(a, own.id, rename, &by_suite).await,
        store
            .update_workspace_app(a, one, catalog.id, rename, &|_| by_suite_in(one))
            .await,
        store
            .update_workspace_app(a, two, own.id, rename, &|_| by_suite_in(two))
            .await,
        store
            .update_workspace_app(b, b_workspace, own.id, rename, &|_| by_suite_in(b_workspace))
            .await,
        store
            .update_workspace_app(b, one, own.id, rename, &|_| by_suite_in(one))
            .await,
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

    let updated = store
        .update_organization_app(a, catalog.id, rename, &by_suite)
        .await
        .unwrap();
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
                description: Some(""),
                ..AppChanges::default()
            },
            &|_| by_suite_in(one),
        )
        .await
        .unwrap();
    assert_eq!((described.name.as_str(), described.description.as_str()), ("Echo", ""));
    assert_eq!(
        store.find_workspace_app(a, one, own.id).await.unwrap(),
        Some(described.clone())
    );
    assert_eq!(described.version, own.version, "a rename keeps the version");

    // A new template replaces the old and raises the version, at either level.
    let template = AuthTemplate::OAuth2 {
        authorize_url: Some("https://auth.example/authorize".to_owned()),
        token_url: Some("https://auth.example/token".to_owned()),
        available_scopes: Vec::new(),
        pkce: false,
        identity_url: None,
    };
    let templated = store
        .update_organization_app(
            a,
            catalog.id,
            AppChanges {
                auth: Some(&template),
                ..AppChanges::default()
            },
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!((&templated.auth, templated.version), (&template, updated.version + 1));
    assert_eq!(templated.name, "Renamed", "the rest is kept");
    let own_templated = store
        .update_workspace_app(
            a,
            one,
            own.id,
            AppChanges {
                auth: Some(&template),
                ..AppChanges::default()
            },
            &|_| by_suite_in(one),
        )
        .await
        .unwrap();
    assert_eq!(
        (&own_templated.auth, own_templated.version),
        (&template, own.version + 1)
    );
    assert_eq!(
        store.find_workspace_app(a, one, own.id).await.unwrap(),
        Some(own_templated)
    );
}

async fn refuses_a_workspace_of_another_organization(store: &dyn AppStore, prepare: &Prepare) {
    let (a, _, _) = organization(prepare).await;
    let (_, b_workspace, _) = organization(prepare).await;
    let result = store
        .create_workspace_app(a, b_workspace, AppId::generate(), &new("Sneaky", "sneaky"), &|_| {
            by_suite_in(b_workspace)
        })
        .await;
    assert_eq!(result, Err(AppError::NotFound));
    assert_eq!(store.list_workspace_apps(a, b_workspace).await.unwrap(), []);
}

async fn forgets_only_the_failed_registration(store: &dyn AppStore, prepare: &Prepare) {
    let (org, one, two) = organization(prepare).await;
    let (foreign, _, _) = organization(prepare).await;
    let catalog = catalog_app(
        store,
        org,
        NewApp::blank("Board".into(), "board".into(), String::new(), AuthType::McpOAuth),
    )
    .await;
    let own = workspace_app(
        store,
        org,
        one,
        NewApp::blank("Board".into(), "board".into(), String::new(), AuthType::McpOAuth),
    )
    .await;
    let mut original = registration(ISSUER, "first", None);
    original.server.response_iss_supported = true;
    let saved = store
        .save_organization_client_registration(org, catalog.id, Environment::Sandbox, &original)
        .await
        .unwrap();
    assert!(saved.server.response_iss_supported);
    store
        .save_workspace_client_registration(org, one, own.id, &original)
        .await
        .unwrap();
    for (organization, app, environment, issuer, client) in [
        (foreign, catalog.id, Environment::Sandbox, ISSUER, "first"),
        (org, catalog.id, Environment::Production, ISSUER, "first"),
        (org, own.id, Environment::Sandbox, ISSUER, "first"),
        (org, catalog.id, Environment::Sandbox, "https://other.example", "first"),
        (org, catalog.id, Environment::Sandbox, ISSUER, "stale"),
    ] {
        store
            .forget_organization_client_registration(organization, app, environment, issuer, client)
            .await
            .unwrap();
        assert_eq!(
            store
                .find_organization_client_registration(org, catalog.id, Environment::Sandbox, ISSUER)
                .await
                .unwrap(),
            Some(saved.clone())
        );
    }
    for (organization, workspace, app, issuer, client) in [
        (foreign, one, own.id, ISSUER, "first"),
        (org, two, own.id, ISSUER, "first"),
        (org, one, catalog.id, ISSUER, "first"),
        (org, one, own.id, "https://other.example", "first"),
        (org, one, own.id, ISSUER, "stale"),
    ] {
        store
            .forget_workspace_client_registration(organization, workspace, app, issuer, client)
            .await
            .unwrap();
        assert!(
            store
                .find_workspace_client_registration(org, one, own.id, ISSUER)
                .await
                .unwrap()
                .is_some()
        );
    }
    store
        .forget_organization_client_registration(org, catalog.id, Environment::Sandbox, ISSUER, "first")
        .await
        .unwrap();
    store
        .forget_workspace_client_registration(org, one, own.id, ISSUER, "first")
        .await
        .unwrap();
    assert!(
        store
            .find_organization_client_registration(org, catalog.id, Environment::Sandbox, ISSUER)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .find_workspace_client_registration(org, one, own.id, ISSUER)
            .await
            .unwrap()
            .is_none()
    );
    let next = registration(ISSUER, "second", None);
    store
        .save_organization_client_registration(org, catalog.id, Environment::Sandbox, &next)
        .await
        .unwrap();
    store
        .save_workspace_client_registration(org, one, own.id, &next)
        .await
        .unwrap();
    store
        .forget_organization_client_registration(org, catalog.id, Environment::Sandbox, ISSUER, "first")
        .await
        .unwrap();
    store
        .forget_workspace_client_registration(org, one, own.id, ISSUER, "first")
        .await
        .unwrap();
    assert_eq!(
        store
            .find_organization_client_registration(org, catalog.id, Environment::Sandbox, ISSUER)
            .await
            .unwrap()
            .unwrap()
            .client_id,
        "second"
    );
    assert_eq!(
        store
            .find_workspace_client_registration(org, one, own.id, ISSUER)
            .await
            .unwrap()
            .unwrap()
            .client_id,
        "second"
    );
}
