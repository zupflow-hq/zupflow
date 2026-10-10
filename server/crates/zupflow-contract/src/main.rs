//! `zupflow-contract-server`: what the shared contract suite runs against.
//! Development only: it signs in fixed personas by token, so it is never part
//! of a release, and it only ever listens on loopback.
//!
//! ```text
//! zupflow-contract-server serve <database>          the API, personas signed in by token; prints `listening <port>`
//! zupflow-contract-server reset <database>          restore the reference fixture
//! zupflow-contract-server operations                the operations the API serves, comma-separated
//! ```
//!
//! A persona's token is `contract:{persona}`: `owner`, `developer` and
//! `viewer` belong to `northwind`, `outsider` owns `larkspur`. `reset` runs
//! beside a serving process, against the same database file.

mod fixture_apps;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

// Development only: a release build of this binary is a mistake, so it fails.
#[cfg(not(debug_assertions))]
compile_error!("zupflow-contract is for development only and is never built for release");

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use zupflow_basic::local_keys::LocalKeyService;
use zupflow_core::api_keys::{ApiKeyHasher, ApiKeyStore, NewApiKey, NewSecret};
use zupflow_core::apps::AppStore;
use zupflow_core::connect::{ConnectIntent, ConnectStore, NewConnectSession, Reference};
use zupflow_core::environment::Environment;
use zupflow_core::idempotency::Fingerprinter;
use zupflow_core::identity::{Identity, IdentityError, Profile, VerifiedIdentity};
use zupflow_core::ids::{ApiKeyId, AppId, ConnectSessionId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{InstallationStore, InstalledApp, TemplateSnapshot};
use zupflow_core::organizations::{MemberRole, OrganizationStore, User};
use zupflow_core::outbound::{Limits, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse, Refusal};
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::{NewWorkspace, WorkspaceStore};
use zupflow_server::{AppState, RequestGuard};
use zupflow_sqlite::{
    Database, SqliteApiKeyStore, SqliteAppStore, SqliteConnectStore, SqliteIdempotencyStore, SqliteInstallationStore,
    SqliteOrganizationStore, SqliteWorkspaceStore,
};

/// The personas, with the address each signs in with: people's names, as in
/// the mock, so the addresses the suite invites (`viewer@…`, `contract@…`)
/// are never taken.
const PERSONAS: [(&str, &str); 4] = [
    ("owner", "jordan@northwind.example"),
    ("developer", "priya@northwind.example"),
    ("viewer", "sam@northwind.example"),
    ("outsider", "riley@larkspur.example"),
];

/// Signs in `Bearer contract:{persona}` as that persona; anything else is 401.
struct PersonaIdentity;

fn persona(name: &str) -> Option<VerifiedIdentity> {
    let (name, email) = PERSONAS.into_iter().find(|(persona, _)| *persona == name)?;
    let mut display = name.to_owned();
    display[..1].make_ascii_uppercase();
    Some(VerifiedIdentity {
        provider: "contract".to_owned(),
        subject: name.to_owned(),
        profile: Profile {
            name: display,
            email: email.to_owned(),
            email_verified: true,
            avatar_url: None,
        },
    })
}

#[async_trait]
impl Identity for PersonaIdentity {
    async fn verify(&self, bearer: Option<&str>) -> Result<VerifiedIdentity, IdentityError> {
        bearer
            .and_then(|token| token.strip_prefix("contract:"))
            .and_then(persona)
            .ok_or(IdentityError::Unauthenticated)
    }
}

/// The providers the suite's apps verify credentials with, as the mock
/// imitates them: nothing leaves the machine. A credential containing
/// `invalid` is rejected with `401`; any other is accepted.
struct FixtureProviders;

#[async_trait]
impl OutboundHttp for FixtureProviders {
    async fn send(&self, request: OutboundRequest, _: Limits) -> Result<OutboundResponse, OutboundError> {
        if !request.url.starts_with("https://") {
            return Err(OutboundError::Refused(Refusal::NotHttps));
        }
        let rejected = request
            .credential
            .as_ref()
            .is_some_and(|credential| credential.value.to_ascii_lowercase().contains("invalid"));
        Ok(OutboundResponse {
            status: if rejected { 401 } else { 200 },
            headers: Vec::new(),
            body: b"{}".to_vec(),
        })
    }
}

async fn open(path: &str) -> Result<Database, String> {
    Database::open(Path::new(path))
        .await
        .map_err(|error| format!("can't open {path}: {error}"))
}

/// Serves on a loopback port the system picks, announced on stdout as
/// `listening <port>`, so a runner never tests whatever else holds a port.
async fn serve(path: &str) -> Result<(), String> {
    let database = open(path).await?;
    let secret = database
        .instance_secret("secret_key")
        .await
        .map_err(|error| error.to_string())?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("can't listen on loopback: {error}"))?;
    let port = listener.local_addr().map_err(|error| error.to_string())?.port();
    let state = AppState {
        identity: Arc::new(PersonaIdentity),
        organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
        workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
        apps: Arc::new(SqliteAppStore::new(database.clone())),
        installations: Arc::new(SqliteInstallationStore::new(database.clone())),
        connect: Arc::new(SqliteConnectStore::new(database.clone())),
        keys: Arc::new(LocalKeyService::from_server_secret(&secret)),
        outbound: Arc::new(FixtureProviders),
        public_url: format!("http://localhost:{port}"),
        api_keys: Arc::new(SqliteApiKeyStore::new(database.clone())),
        api_key_hasher: Arc::new(ApiKeyHasher::new(secret)),
        idempotency: Arc::new(SqliteIdempotencyStore::new(database)),
        fingerprinter: Arc::new(Fingerprinter::new(secret)),
    };
    #[allow(clippy::print_stdout)]
    {
        println!("listening {port}");
    }
    // Every persona's token is public, as `local`'s owner is: the same guard
    // keeps a rebound name or another site out.
    axum::serve(listener, zupflow_server::app(state, RequestGuard::local(port, &[])))
        .await
        .map_err(|error| error.to_string())
}

/// Restores the reference fixture: everything the API serves so far, as the
/// contract suite's `Target` describes it.
async fn reset(path: &str) -> Result<(), String> {
    let database = open(path).await?;
    let fail = |what: &str| {
        let what = what.to_owned();
        move |error: &dyn std::fmt::Debug| format!("{what}: {error:?}")
    };
    // Wipe what tenants hold. Cascades take members, workspaces, keys, jobs and identities.
    let options = SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePool::connect_with(options)
        .await
        .map_err(|error| fail("connect")(&error))?;
    for statement in ["DELETE FROM organizations", "DELETE FROM users"] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .map_err(|error| fail(statement)(&error))?;
    }

    let organizations = SqliteOrganizationStore::new(database.clone());
    let mut users: Vec<(&str, User)> = Vec::new();
    for (name, _) in PERSONAS {
        let identity = persona(name).ok_or("persona")?;
        let user = organizations
            .user_for_identity(&identity)
            .await
            .map_err(|error| fail("user")(&error))?;
        users.push((name, user));
    }
    let user = |name: &str| {
        users
            .iter()
            .find(|(persona, _)| *persona == name)
            .map(|(_, user)| user.clone())
    };
    let (owner, outsider) = (user("owner").ok_or("owner")?, user("outsider").ok_or("outsider")?);

    let (northwind, _) = organizations
        .create_organization(OrganizationId::generate(), "Northwind", "northwind", owner.id)
        .await
        .map_err(|error| fail("northwind")(&error))?;
    let (larkspur, _) = organizations
        .create_organization(OrganizationId::generate(), "Larkspur", "larkspur", outsider.id)
        .await
        .map_err(|error| fail("larkspur")(&error))?;
    for (name, role) in [("developer", MemberRole::Developer), ("viewer", MemberRole::Viewer)] {
        let member = user(name).ok_or("member")?;
        organizations
            .invite_member(northwind.id, &member.email, role)
            .await
            .map_err(|error| fail("invite")(&error))?;
        // Accepted at once: the API has no acceptance route yet.
        sqlx::query(
            "UPDATE members SET status = 'active', joined_at = invited_at \
             WHERE organization_id = ? AND user_id = ?",
        )
        .bind(northwind.id.uuid())
        .bind(member.id.uuid())
        .execute(&pool)
        .await
        .map_err(|error| fail("accept")(&error))?;
    }
    pool.close().await;

    let workspaces = SqliteWorkspaceStore::new(database.clone());
    let mut workspace_ids = Vec::new();
    for (organization, name, slug, environment) in [
        (northwind.id, "Acme", "acme", Environment::Production),
        (northwind.id, "Beta Labs", "beta-labs", Environment::Production),
        (northwind.id, "Acme Sandbox", "acme-sandbox", Environment::Sandbox),
        (northwind.id, "QA Playground", "qa-playground", Environment::Sandbox),
        (larkspur.id, "Harbor", "harbor", Environment::Production),
        (larkspur.id, "Harbor Sandbox", "harbor-sandbox", Environment::Sandbox),
    ] {
        let workspace = NewWorkspace {
            name: name.to_owned(),
            slug: slug.to_owned(),
            environment,
            external_id: None,
        };
        let created = workspaces
            .create_workspace(organization, WorkspaceId::generate(), &workspace)
            .await
            .map_err(|error| fail(slug)(&error))?;
        workspace_ids.push((slug, created.id, environment));
    }
    let workspace_id = |slug: &str| {
        workspace_ids
            .iter()
            .find(|(each, _, _)| *each == slug)
            .map(|(_, id, environment)| (*id, *environment))
            .ok_or_else(|| format!("workspace {slug}"))
    };

    let apps = SqliteAppStore::new(database.clone());
    // Who is ready where, as the suite's `Target` describes it.
    let secret_key = database
        .instance_secret("secret_key")
        .await
        .map_err(|error| error.to_string())?;
    let sealing = LocalKeyService::from_server_secret(&secret_key);
    let ready = |url: &str, oauth: bool| fixture_apps::Ready {
        mcp_server_url: url.to_owned(),
        oauth,
    };
    // Each app as it was made, to install it below.
    let mut seeded: Vec<(OrganizationId, String, InstalledApp)> = Vec::new();
    for (organization, app, sandbox, production) in [
        (
            northwind.id,
            fixture_apps::google_workspace()?,
            Some(ready("https://mcp.northwind.example/google-workspace", true)),
            None,
        ),
        (
            northwind.id,
            fixture_apps::linear()?,
            Some(ready("https://mcp.linear.example/mcp", true)),
            // Incomplete in Production: a server, no client.
            Some(fixture_apps::Ready {
                mcp_server_url: "https://mcp.linear.example/mcp".to_owned(),
                oauth: false,
            }),
        ),
        (northwind.id, fixture_apps::notion()?, None, None),
        (
            larkspur.id,
            fixture_apps::linear()?,
            Some(ready("https://mcp.linear.example/mcp", true)),
            None,
        ),
    ] {
        let created = apps
            .create_organization_app(organization, AppId::generate(), &app)
            .await
            .map_err(|error| fail(&app.slug)(&error))?;
        seeded.push((organization, app.slug.clone(), InstalledApp::Catalog(created.id)));
        for (environment, configuration) in [(Environment::Sandbox, sandbox), (Environment::Production, production)] {
            let Some(configuration) = configuration else { continue };
            let changes = configuration
                .changes(&sealing, organization, None, environment, created.id)
                .await?;
            apps.save_organization_configuration(organization, created.id, environment, &changes)
                .await
                .map_err(|error| fail(&app.slug)(&error))?;
        }
    }
    for (organization, workspace, app) in [
        (
            northwind.id,
            "acme",
            fixture_apps::publishing("Acme Publishing", "AP", "acme-publishing", "cms.acme.example")?,
        ),
        (northwind.id, "qa-playground", fixture_apps::echo_server()?),
        (
            larkspur.id,
            "harbor",
            fixture_apps::publishing("Harbor Publishing", "HP", "harbor-publishing", "cms.harbor.example")?,
        ),
    ] {
        let (workspace, environment) = workspace_id(workspace)?;
        let created = apps
            .create_workspace_app(organization, workspace, AppId::generate(), &app)
            .await
            .map_err(|error| fail(&app.slug)(&error))?;
        seeded.push((organization, app.slug.clone(), InstalledApp::Workspace(created.id)));
        // Token apps: ready with a server alone.
        let changes = ready(&format!("https://{}.example/mcp", app.slug), false)
            .changes(&sealing, organization, Some(workspace), environment, created.id)
            .await?;
        apps.save_workspace_configuration(organization, workspace, created.id, &changes)
            .await
            .map_err(|error| fail(&app.slug)(&error))?;
    }

    // Connected installations, each with its credential sealed as Connect seals it.
    let installations = SqliteInstallationStore::new(database.clone());
    for (organization, workspace, slug, label) in [
        (northwind.id, "acme", "google-workspace", "Marketing"),
        (northwind.id, "beta-labs", "google-workspace", "Google"),
        (northwind.id, "acme-sandbox", "linear", "Engineering"),
        (northwind.id, "qa-playground", "echo-server", "Echo"),
        (larkspur.id, "harbor", "linear", "Harbor"),
    ] {
        let (workspace, environment) = workspace_id(workspace)?;
        let app = seeded
            .iter()
            .find(|(owner, seeded, _)| *owner == organization && seeded == slug)
            .map(|(_, _, app)| *app)
            .ok_or_else(|| format!("app {slug}"))?;
        let found = match app {
            InstalledApp::Catalog(id) => apps.find_organization_app(organization, id).await,
            InstalledApp::Workspace(id) => apps.find_workspace_app(organization, workspace, id).await,
        }
        .map_err(|error| fail(slug)(&error))?
        .ok_or_else(|| format!("app {slug}"))?;
        let installation_id = InstallationId::generate();
        let installation = fixture_apps::installation(
            &sealing,
            TemplateSnapshot {
                auth: found.auth,
                app_version: found.version,
            },
            fixture_apps::Owner {
                organization_id: organization,
                workspace_id: workspace,
                environment,
                installation_id,
            },
            app,
            label,
        )
        .await?;
        installations
            .create_installation(organization, workspace, installation_id, &installation)
            .await
            .map_err(|error| fail(label)(&error))?;
    }

    // Connect settings: Northwind's Sandbox allows the suite's return URL,
    // Larkspur's doesn't. And a session in each workspace the suite expects one.
    let connect = SqliteConnectStore::new(database.clone());
    for (organization, return_urls, origins) in [
        (
            northwind.id,
            vec!["http://localhost:3000/settings/integrations".to_owned()],
            vec!["http://localhost:3000".to_owned()],
        ),
        (
            larkspur.id,
            vec!["https://harbor.example/settings/integrations".to_owned()],
            vec!["https://harbor.example".to_owned()],
        ),
    ] {
        connect
            .save_settings(organization, Environment::Sandbox, &return_urls, &origins)
            .await
            .map_err(|error| fail("connect settings")(&error))?;
    }
    for (organization, workspace, slug) in [
        (northwind.id, "acme", "google-workspace"),
        (northwind.id, "beta-labs", "google-workspace"),
        (larkspur.id, "harbor", "linear"),
    ] {
        let (workspace, environment) = workspace_id(workspace)?;
        let app = seeded
            .iter()
            .find(|(owner, seeded, _)| *owner == organization && seeded == slug)
            .map(|(_, _, app)| *app)
            .ok_or_else(|| format!("app {slug}"))?;
        let reference = Reference::generate().map_err(|error| fail("reference")(&error))?;
        connect
            .create_session(
                organization,
                workspace,
                ConnectSessionId::generate(),
                &NewConnectSession {
                    environment,
                    app,
                    configuration_version: 1,
                    intent: ConnectIntent::Create,
                    installation_id: None,
                    label: "Team".to_owned(),
                    return_url: None,
                    requested_scopes: Vec::new(),
                    reference_hash: reference.hash(),
                },
                Timestamp::now(),
            )
            .await
            .map_err(|error| fail("session")(&error))?;
    }

    let secret = database
        .instance_secret("secret_key")
        .await
        .map_err(|error| error.to_string())?;
    let hasher = ApiKeyHasher::new(secret);
    let keys = SqliteApiKeyStore::new(database);
    for (organization, creator) in [(northwind.id, &owner), (larkspur.id, &outsider)] {
        let secret = NewSecret::generate(Environment::Sandbox).map_err(|error| fail("secret")(&error))?;
        let key = NewApiKey {
            environment: Environment::Sandbox,
            name: "Fixture".to_owned(),
            prefix: secret.prefix.clone(),
            last4: secret.last4.clone(),
            secret_hash: hasher.hash(&secret.secret),
            created_by: creator.id,
        };
        keys.create_api_key(organization, ApiKeyId::generate(), &key)
            .await
            .map_err(|error| fail("key")(&error))?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["serve", database] => serve(database).await,
        ["reset", database] => reset(database).await,
        ["operations"] => {
            #[allow(clippy::print_stdout)]
            {
                println!("{}", zupflow_server::SERVED_OPERATIONS.join(","));
            }
            Ok(())
        }
        _ => Err("usage: zupflow-contract-server serve <database> | reset <database> | operations".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("zupflow-contract-server: {message}");
            ExitCode::FAILURE
        }
    }
}
