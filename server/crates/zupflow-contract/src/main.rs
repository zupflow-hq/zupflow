//! `zupflow-contract-server`: what the shared contract suite runs against.
//! Development only: it signs in fixed personas by token, so it is never part
//! of a release, and it only ever listens on loopback.
//!
//! ```text
//! zupflow-contract-server serve <database>          the API, personas signed in by token; prints `listening <port>`
//! zupflow-contract-server serve <database> --console <origin>
//!                                                   the same, for a Console served from <origin>: CORS for it,
//!                                                   and a request without a token is the owner, as with `local`
//! zupflow-contract-server serve <database> --local-app <host>:<port>=<certificate.pem> [...]
//!                                                   live: outbound calls reach the named local apps (and
//!                                                   nothing else), the scripted model answers runs, and
//!                                                   the background workers run, as in `zupflow`; only
//!                                                   when built with the `local-apps` feature
//! zupflow-contract-server reset <database>          restore the reference fixture
//! zupflow-contract-server expire-connect <database> <organization> <workspace> <session>
//!                                                   expire one pending session in that exact scope
//! zupflow-contract-server operations                the operations the API serves, comma-separated
//! ```
//!
//! A persona's token is `contract:{persona}`: `owner`, `developer` and
//! `viewer` belong to `northwind`, `outsider` owns `larkspur`. `reset` runs
//! beside a serving process, against the same database file.
//!
//! `--local-app` is how a fake app served on this machine, such as Taskboard
//! (`zupflow-taskboard`), is reached for an end-to-end run or a click-through:
//! its name must be under the reserved `.example` domain, it's pinned to
//! loopback and its port, and only the certificates given are trusted
//! (`SafeHttpClient::for_local_apps`, development only, behind this crate's
//! opt-in `local-apps` feature, which nothing turns on by default). Without
//! it, nothing
//! leaves the machine: the fixture's providers answer from memory, no model
//! answers runs, and no background work runs.

mod fixture_apps;
mod fixture_runs;
mod fixture_webhooks;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

// Development only: a release build of this binary is a mistake, so it fails.
#[cfg(not(debug_assertions))]
compile_error!("zupflow-contract is for development only and is never built for release");

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use zupflow_basic::configured_models::ConfiguredModels;
use zupflow_basic::local_keys::LocalKeyService;
use zupflow_core::agents::{Agent, AgentStore, Grant, GrantTools, NewAgent};
use zupflow_core::api_keys::{ApiKey, ApiKeyHasher, ApiKeyStore, NewApiKey, NewSecret};
use zupflow_core::apps::{App, AppStore, Configuration, ToolEffect};
use zupflow_core::audit::{AuditAction, AuditActor, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::connect::{ConnectIntent, ConnectSettings, ConnectStore, NewConnectSession, Reference};
use zupflow_core::environment::Environment;
use zupflow_core::idempotency::Fingerprinter;
use zupflow_core::identity::{Identity, IdentityError, Profile, VerifiedIdentity};
use zupflow_core::ids::{
    AgentId, ApiKeyId, AppId, ConnectSessionId, GrantId, InstallationId, OrganizationId, WebhookEndpointId, WorkspaceId,
};
use zupflow_core::installations::{InstallationStore, InstalledApp, TemplateSnapshot};
use zupflow_core::keys::{EncryptionContext, seal};
use zupflow_core::organizations::{INVITATION_TTL_MS, Member, MemberRole, Organization, OrganizationStore, User};
use zupflow_core::outbound::{Limits, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse, Refusal};
use zupflow_core::policy::{AppToolLevel, ToolLevel, ToolPolicyStore};
use zupflow_core::runs::CallTarget;
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{ListedTool, ToolRegistry, ToolRevision};
use zupflow_core::webhooks::{
    NewEndpoint, SECRET_VERSION, WebhookEndpoint, WebhookEventType, WebhookStore, new_signing_secret,
};
use zupflow_core::workspaces::{NewWorkspace, Workspace, WorkspaceStore};
use zupflow_server::sign_in::SignInSettings;
use zupflow_server::{AppState, RequestGuard};
use zupflow_sqlite::{
    Database, SqliteAgentStore, SqliteApiKeyStore, SqliteAppStore, SqliteConnectStore, SqliteIdempotencyStore,
    SqliteInstallationStore, SqliteOrganizationStore, SqliteRunStore, SqliteToolPolicyStore, SqliteToolRegistry,
    SqliteWebhookStore, SqliteWorkspaceStore,
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

/// The models the server offers, as `ZUPFLOW_MODELS` lists them. Fictional:
/// the suite takes whichever model the server lists first.
const MODELS: &str = r#"[
    {"id":"model-balanced","name":"Balanced","provider":"Example","description":"Balanced default for tool-heavy work."},
    {"id":"model-capable","name":"Capable","provider":"Example","description":"Most capable. Long, multi-step tasks."},
    {"id":"model-fast","name":"Fast","provider":"Example","description":"Fast and inexpensive. Simple lookups."}
]"#;

/// Signs in `Bearer contract:{persona}` as that persona; anything else is
/// 401, except that with `signed_out_as_owner` (a browser run, where the
/// Console signs in as with `local`) no token at all is the owner.
struct PersonaIdentity {
    signed_out_as_owner: bool,
}

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
        match bearer {
            None if self.signed_out_as_owner => persona("owner"),
            bearer => bearer
                .and_then(|token| token.strip_prefix("contract:"))
                .and_then(persona),
        }
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

/// How `serve` runs: for a Console at an origin, and live with local apps.
#[derive(Default)]
struct Serving {
    console: Option<String>,
    #[cfg(feature = "local-apps")]
    local_apps: Vec<zupflow_outbound::LocalApp>,
}

impl Serving {
    /// Live: calls leave the process, for the local apps named alone.
    #[cfg(feature = "local-apps")]
    fn live(&self) -> bool {
        !self.local_apps.is_empty()
    }

    /// Never live without the `local-apps` feature.
    #[cfg(not(feature = "local-apps"))]
    fn live(&self) -> bool {
        false
    }

    /// The outbound path: the local apps when live, the fixture's providers
    /// otherwise.
    fn outbound(&self) -> Result<Arc<dyn OutboundHttp>, String> {
        #[cfg(feature = "local-apps")]
        if self.live() {
            let client = zupflow_outbound::SafeHttpClient::for_local_apps(&self.local_apps)
                .map_err(|error| error.to_string())?;
            let apps: Vec<String> = self
                .local_apps
                .iter()
                .map(|app| format!("{}:{}", app.host, app.port))
                .collect();
            eprintln!(
                "development only: outbound calls reach {} at 127.0.0.1, and nothing else",
                apps.join(", ")
            );
            return Ok(Arc::new(client));
        }
        Ok(Arc::new(FixtureProviders))
    }
}

/// `--local-app <host>:<port>=<certificate.pem>`.
#[cfg(feature = "local-apps")]
fn local_app(value: &str) -> Result<zupflow_outbound::LocalApp, String> {
    const USAGE: &str = "--local-app takes <host>:<port>=<certificate.pem>";
    let (address, certificate) = value.split_once('=').ok_or(USAGE)?;
    let (host, port) = address.rsplit_once(':').ok_or(USAGE)?;
    let port = port.parse::<u16>().ok().filter(|port| *port != 0).ok_or(USAGE)?;
    let certificate_pem = std::fs::read(certificate).map_err(|error| format!("can't read {certificate}: {error}"))?;
    Ok(zupflow_outbound::LocalApp {
        host: host.to_owned(),
        port,
        certificate_pem,
    })
}

/// `serve`'s options after the database.
fn serving(options: &[&str]) -> Result<Serving, String> {
    let mut serving = Serving::default();
    let mut options = options.iter();
    while let Some(option) = options.next() {
        let value = options.next().ok_or_else(|| format!("{option} needs a value"))?;
        match *option {
            "--console" if serving.console.is_none() => serving.console = Some((*value).to_owned()),
            #[cfg(feature = "local-apps")]
            "--local-app" => serving.local_apps.push(local_app(value)?),
            #[cfg(not(feature = "local-apps"))]
            "--local-app" => {
                return Err("--local-app needs a build with the `local-apps` feature (development only)".to_owned());
            }
            _ => return Err(format!("unknown option {option}")),
        }
    }
    Ok(serving)
}

/// Serves on a loopback port the system picks, announced on stdout as
/// `listening <port>`, so a runner never tests whatever else holds a port.
async fn serve(path: &str, serving: Serving) -> Result<(), String> {
    let console = serving.console.as_deref();
    let live = serving.live();
    let outbound = serving.outbound()?;
    let models = ConfiguredModels::parse(MODELS).map_err(|error| error.to_string())?;
    let provider: Arc<dyn zupflow_core::models::ModelProvider> = if live {
        let served: Vec<String> = zupflow_core::models::ModelCatalog::list_models(&models)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|model| model.id)
            .collect();
        Arc::new(zupflow_basic::scripted_model::ScriptedModel::serving(served))
    } else {
        Arc::new(zupflow_basic::no_model::NoModelProvider)
    };
    let database = open(path).await?;
    let secret = database
        .instance_secret("secret_key")
        .await
        .map_err(|error| error.to_string())?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("can't listen on loopback: {error}"))?;
    let port = listener.local_addr().map_err(|error| error.to_string())?.port();
    let public_url = format!("http://localhost:{port}");
    let state = AppState {
        clock: Arc::new(zupflow_core::time::SystemClock),
        identity: Arc::new(PersonaIdentity {
            signed_out_as_owner: console.is_some(),
        }),
        organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
        workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
        apps: Arc::new(SqliteAppStore::new(database.clone())),
        installations: Arc::new(SqliteInstallationStore::new(database.clone())),
        agents: Arc::new(SqliteAgentStore::new(database.clone())),
        policies: Arc::new(zupflow_sqlite::SqliteToolPolicyStore::new(database.clone())),
        runs: Arc::new(zupflow_sqlite::SqliteRunStore::new(database.clone())),
        provider,
        approvals: Arc::new(zupflow_basic::configured_policy::ConfiguredPolicy::new(Arc::new(
            zupflow_sqlite::SqliteToolPolicyStore::new(database.clone()),
        ))),
        events: Arc::new(zupflow_basic::in_process_events::InProcessEventBus::default()),
        models: Arc::new(models),
        connect: Arc::new(SqliteConnectStore::new(database.clone())),
        jobs: Arc::new(zupflow_sqlite::SqliteJobQueue::new(database.clone())),
        tools: Arc::new(zupflow_sqlite::SqliteToolRegistry::new(database.clone())),
        cache: Arc::new(zupflow_basic::memory_cache::MemoryCache::default()),
        keys: Arc::new(LocalKeyService::from_server_secret(&secret)),
        outbound,
        public_url: public_url.clone(),
        // The suite's personas are verified identities from a provider of
        // their own, so it says `oidc`, naming a fictional one; a browser
        // run signs in as with `local`.
        sign_in: match console {
            Some(_) => SignInSettings::Local,
            None => SignInSettings::Oidc {
                issuer: "https://sign-in.contract.example".to_owned(),
                client_id: "contract".to_owned(),
            },
        },
        api_keys: Arc::new(SqliteApiKeyStore::new(database.clone())),
        api_key_hasher: Arc::new(ApiKeyHasher::new(secret)),
        webhooks: Arc::new(zupflow_sqlite::SqliteWebhookStore::new(database.clone())),
        audit: Arc::new(zupflow_sqlite::SqliteAuditStore::new(database.clone())),
        idempotency: Arc::new(SqliteIdempotencyStore::new(database)),
        fingerprinter: Arc::new(Fingerprinter::new(secret)),
    };
    #[allow(clippy::print_stdout)]
    {
        println!("listening {port}");
    }
    // Every persona's token is public, as `local`'s owner is: the same guard
    // keeps a rebound name or another site out.
    let guard = match console {
        Some(origin) => RequestGuard::local(port, &[]).with_console(origin),
        None => RequestGuard::local(port, &[]),
    }
    .with_public_url(&public_url);
    // Live, the background work runs in the same process, as in `zupflow`.
    let workers = if live {
        vec![
            tokio::spawn(zupflow_server::discovery::run_worker(state.clone())),
            tokio::spawn(zupflow_server::run_engine::run_worker(state.clone())),
            tokio::spawn(zupflow_server::webhook_delivery::run_workers(state.clone())),
        ]
    } else {
        Vec::new()
    };
    let served = axum::serve(listener, zupflow_server::app(state, guard))
        .await
        .map_err(|error| error.to_string());
    for worker in workers {
        worker.abort();
    }
    served
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

    let (northwind, owner_member) = organizations
        .create_organization(
            OrganizationId::generate(),
            "Northwind",
            "northwind",
            owner.id,
            &|(organization, _): &(Organization, Member)| {
                by_fixture(
                    AuditAction::OrganizationCreated,
                    AuditTarget::new(AuditObject::Organization(organization.id), &organization.name),
                    AuditPlace::Organization,
                )
            },
        )
        .await
        .map_err(|error| fail("northwind")(&error))?;
    let (larkspur, outsider_member) = organizations
        .create_organization(
            OrganizationId::generate(),
            "Larkspur",
            "larkspur",
            outsider.id,
            &|(organization, _): &(Organization, Member)| {
                by_fixture(
                    AuditAction::OrganizationCreated,
                    AuditTarget::new(AuditObject::Organization(organization.id), &organization.name),
                    AuditPlace::Organization,
                )
            },
        )
        .await
        .map_err(|error| fail("larkspur")(&error))?;
    for (name, role) in [("developer", MemberRole::Developer), ("viewer", MemberRole::Viewer)] {
        let member = user(name).ok_or("member")?;
        let invitation = organizations
            .invite_member(
                northwind.id,
                &member.email,
                role,
                owner_member.id,
                &|invited: &Member| {
                    by_fixture(
                        AuditAction::MemberInvited,
                        AuditTarget::new(AuditObject::Member(invited.id), &invited.user.email),
                        AuditPlace::Organization,
                    )
                },
            )
            .await
            .map_err(|error| fail("invite")(&error))?;
        organizations
            .accept_invitation_of_user(member.id, invitation.id, Timestamp::now(), &|(_, joined): &(
                Organization,
                Member,
            )| {
                by_fixture(
                    AuditAction::MemberJoined,
                    AuditTarget::new(AuditObject::Member(joined.id), &joined.user.email),
                    AuditPlace::Organization,
                )
            })
            .await
            .map_err(|error| fail("accept")(&error))?;
    }
    // Larkspur invited the viewer long enough ago that the invitation expired.
    let viewer = user("viewer").ok_or("viewer")?;
    let expired = organizations
        .invite_member(
            larkspur.id,
            &viewer.email,
            MemberRole::Developer,
            outsider_member.id,
            &|invited: &Member| {
                by_fixture(
                    AuditAction::MemberInvited,
                    AuditTarget::new(AuditObject::Member(invited.id), &invited.user.email),
                    AuditPlace::Organization,
                )
            },
        )
        .await
        .map_err(|error| fail("invite")(&error))?;
    sqlx::query("UPDATE members SET invited_at = ? WHERE organization_id = ? AND id = ?")
        .bind(Timestamp::now().millis() - INVITATION_TTL_MS - 3 * 24 * 60 * 60 * 1000)
        .bind(larkspur.id.uuid())
        .bind(expired.id.uuid())
        .execute(&pool)
        .await
        .map_err(|error| fail("expire")(&error))?;
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
            .create_workspace(
                organization,
                WorkspaceId::generate(),
                &workspace,
                &|created: &Workspace| {
                    by_fixture(
                        AuditAction::WorkspaceCreated,
                        AuditTarget::new(AuditObject::Workspace(created.id), &created.name),
                        AuditPlace::Workspace(created.id),
                    )
                },
            )
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
            fixture_apps::taskboard()?,
            Some(ready("https://taskboard.example/mcp", true)),
            // Incomplete in Production: a server, no client.
            Some(fixture_apps::Ready {
                mcp_server_url: "https://taskboard.example/mcp".to_owned(),
                oauth: false,
            }),
        ),
        (northwind.id, fixture_apps::notion()?, None, None),
        (
            larkspur.id,
            fixture_apps::taskboard()?,
            Some(ready("https://taskboard.example/mcp", true)),
            None,
        ),
    ] {
        let created = apps
            .create_organization_app(organization, AppId::generate(), &app, &|created: &App| {
                by_fixture(
                    AuditAction::AppCreated,
                    AuditTarget::new(AuditObject::App(created.id), &created.name),
                    AuditPlace::Organization,
                )
            })
            .await
            .map_err(|error| fail(&app.slug)(&error))?;
        seeded.push((organization, app.slug.clone(), InstalledApp::Catalog(created.id)));
        for (environment, configuration) in [(Environment::Sandbox, sandbox), (Environment::Production, production)] {
            let Some(configuration) = configuration else { continue };
            let changes = configuration
                .changes(&sealing, organization, None, environment, created.id)
                .await?;
            apps.save_organization_configuration(
                organization,
                created.id,
                environment,
                &changes,
                &|_: &Configuration| {
                    by_fixture(
                        AuditAction::AppConfigurationUpdated,
                        AuditTarget::new(AuditObject::App(created.id), &created.name),
                        AuditPlace::Environment(environment),
                    )
                },
            )
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
            .create_workspace_app(organization, workspace, AppId::generate(), &app, &|created: &App| {
                by_fixture(
                    AuditAction::AppCreated,
                    AuditTarget::new(AuditObject::App(created.id), &created.name),
                    AuditPlace::Workspace(workspace),
                )
            })
            .await
            .map_err(|error| fail(&app.slug)(&error))?;
        seeded.push((organization, app.slug.clone(), InstalledApp::Workspace(created.id)));
        // Token apps: ready with a server alone.
        let changes = ready(&format!("https://{}.example/mcp", app.slug), false)
            .changes(&sealing, organization, Some(workspace), environment, created.id)
            .await?;
        apps.save_workspace_configuration(organization, workspace, created.id, &changes, &|_: &Configuration| {
            by_fixture(
                AuditAction::AppConfigurationUpdated,
                AuditTarget::new(AuditObject::App(created.id), &created.name),
                AuditPlace::Workspace(workspace),
            )
        })
        .await
        .map_err(|error| fail(&app.slug)(&error))?;
    }

    // Connected installations, each with its credential sealed as Connect seals it.
    let installations = SqliteInstallationStore::new(database.clone());
    let tools = SqliteToolRegistry::new(database.clone());
    // Levels suggested for a catalog app's tools as each listing names them, as discovery does.
    let policies = SqliteToolPolicyStore::new(database.clone());
    let suggest = |organization: OrganizationId, app: InstalledApp, listing: &[ListedTool]| {
        let listed: Vec<_> = listing.iter().map(|tool| (tool.name.clone(), tool.effect)).collect();
        let policies = &policies;
        async move {
            let InstalledApp::Catalog(app) = app else {
                return Ok::<(), String>(());
            };
            policies
                .suggest_levels(organization, app, &listed, Timestamp::now())
                .await
                .map(|_| ())
                .map_err(|error| fail("levels")(&error))
        }
    };
    let mut installed: Vec<(OrganizationId, WorkspaceId, InstallationId, &str)> = Vec::new();
    for (organization, workspace, slug, label) in [
        (northwind.id, "acme", "google-workspace", "Marketing"),
        (northwind.id, "beta-labs", "google-workspace", "Google"),
        (northwind.id, "acme-sandbox", "taskboard", "Engineering"),
        (northwind.id, "qa-playground", "echo-server", "Echo"),
        (larkspur.id, "harbor", "taskboard", "Harbor"),
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
                client: None,
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
        installed.push((organization, workspace, installation_id, label));
        // Discovered as connected: the app's own tools, each as first listed.
        // The Sandbox's Engineering has listings of its own, below.
        if label == "Engineering" {
            continue;
        }
        let listing: Vec<ListedTool> = found
            .tools
            .iter()
            .map(|tool| fixture_tool(&tool.name, &tool.title, &tool.description, tool.effect))
            .collect();
        tools
            .record_listing(organization, workspace, installation_id, &listing, Timestamp::now())
            .await
            .map_err(|error| fail("tools")(&error))?;
        suggest(organization, app, &listing).await?;
    }

    // The Sandbox's Taskboard history matches the mock: one current tool,
    // a description change, a hints-only change, and a synthetic gone tool.
    // Each listing suggests levels too, as discovery does.
    let (organization, workspace, installation_id, _) = installed
        .iter()
        .find(|(_, _, _, label)| *label == "Engineering")
        .copied()
        .ok_or("installation Engineering")?;
    let current: Vec<ListedTool> = fixture_apps::taskboard()?
        .tools
        .iter()
        .map(|tool| fixture_tool(&tool.name, &tool.title, &tool.description, tool.effect))
        .collect();
    let mut older = current.clone();
    for tool in &mut older {
        let mut definition: Value =
            serde_json::from_str(&tool.revision.definition).map_err(|error| error.to_string())?;
        match tool.name.as_str() {
            "create_task" => definition["description"] = json!("Adds a task."),
            "delete_task" => definition["annotations"] = json!({ "readOnlyHint": false }),
            _ => continue,
        }
        tool.revision = fixture_revision(&definition);
    }
    // A synthetic historical tombstone exercises absence; Taskboard serves no export_tasks tool.
    older.push(fixture_tool(
        "export_tasks",
        "Export tasks",
        "Download the board's tasks as a CSV file.",
        ToolEffect::Read,
    ));
    let taskboard = seeded
        .iter()
        .find(|(owner, seeded, _)| *owner == organization && *seeded == "taskboard")
        .map(|(_, _, app)| *app)
        .ok_or("app taskboard")?;
    for (listing, at) in [
        (older, Timestamp::from_millis(Timestamp::now().millis() - 86_400_000)),
        (current, Timestamp::now()),
    ] {
        tools
            .record_listing(organization, workspace, installation_id, &listing, at)
            .await
            .map_err(|error| fail("tools")(&error))?;
        suggest(organization, taskboard, &listing).await?;
    }

    // The organizations' confirmed levels, as people set them: every tool
    // `acme`'s grants hold stays inside them; `list_events` stays suggested.
    // Taskboard's confirmed levels exercise policy independently of hints;
    // no seeded grant includes its destructive delete_task tool.
    for (organization, slug, writer, levels) in [
        (
            northwind.id,
            "google-workspace",
            owner.id,
            &[
                ("search_files", ToolLevel::Allow),
                ("read_document", ToolLevel::Allow),
                ("create_document", ToolLevel::Allow),
                ("send_email", ToolLevel::Approve),
                ("delete_file", ToolLevel::Deny),
            ][..],
        ),
        (
            northwind.id,
            "taskboard",
            owner.id,
            &[
                ("list_tasks", ToolLevel::Allow),
                ("create_task", ToolLevel::Allow),
                ("delete_task", ToolLevel::Allow),
            ],
        ),
        (
            larkspur.id,
            "taskboard",
            outsider.id,
            &[("list_tasks", ToolLevel::Allow), ("create_task", ToolLevel::Allow)],
        ),
    ] {
        let Some(InstalledApp::Catalog(app)) = seeded
            .iter()
            .find(|(owner, seeded, _)| *owner == organization && *seeded == slug)
            .map(|(_, _, app)| *app)
        else {
            return Err(format!("catalog app {slug}"));
        };
        let levels: Vec<(String, ToolLevel)> = levels
            .iter()
            .map(|(tool, level)| ((*tool).to_owned(), *level))
            .collect();
        policies
            .set_levels(
                organization,
                app,
                &levels,
                Some(writer),
                Timestamp::now(),
                &|_: &Vec<AppToolLevel>| {
                    by_fixture(
                        AuditAction::AppToolLevelsUpdated,
                        AuditTarget::new(AuditObject::App(app), slug),
                        AuditPlace::Organization,
                    )
                },
            )
            .await
            .map_err(|error| fail("levels")(&error))?;
    }

    // Agents, as the mock seeds them: in each workspace the suite expects
    // one, an active agent granted tools of its installation; in `acme`, a
    // paused one with none too.
    let agents = SqliteAgentStore::new(database.clone());
    let installation_of = |label: &str| {
        installed
            .iter()
            .find(|(_, _, _, installed)| *installed == label)
            .copied()
            .ok_or_else(|| format!("installation {label}"))
    };
    let mut seeded_agents: Vec<(&str, OrganizationId, WorkspaceId, InstallationId, AgentId, &str)> = Vec::new();
    for (label, name, model, granted, approval) in [
        (
            "Marketing",
            "Publishing assistant",
            "model-balanced",
            &["search_files", "read_document", "create_document"][..],
            &[][..],
        ),
        (
            "Google",
            "Ops assistant",
            "model-balanced",
            &["list_events", "search_files", "read_document", "send_email"],
            &["send_email"],
        ),
        (
            "Engineering",
            "Product scribe",
            "model-balanced",
            &["list_tasks", "create_task"],
            &[],
        ),
        (
            "Echo",
            "Echo bot",
            "model-fast",
            &["echo", "echo_write"],
            &["echo_write"],
        ),
        ("Harbor", "Harbor assistant", "model-balanced", &["list_tasks"], &[]),
    ] {
        let (organization, workspace, installation_id, _) = installation_of(label)?;
        let agent = agents
            .create_agent(
                organization,
                workspace,
                AgentId::generate(),
                &NewAgent {
                    name: name.to_owned(),
                    description: format!("{name}, for the contract suite."),
                    model: model.to_owned(),
                    instructions: "Be brief.".to_owned(),
                },
                &|created: &Agent| {
                    by_fixture(
                        AuditAction::AgentCreated,
                        AuditTarget::new(AuditObject::Agent(created.id), &created.name),
                        AuditPlace::Workspace(workspace),
                    )
                },
            )
            .await
            .map_err(|error| fail(name)(&error))?;
        seeded_agents.push((label, organization, workspace, installation_id, agent.id, model));
        let names = |tools: &[&str]| tools.iter().map(|tool| (*tool).to_owned()).collect::<Vec<_>>();
        if label == "Google" {
            // Made before its levels were set: `list_events` is only
            // suggested, so the store would refuse it now. Written as it
            // stands, since tightening rewrites no grant.
            let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(path).foreign_keys(true))
                .await
                .map_err(|error| fail("connect")(&error))?;
            let json = |tools: &[&str]| serde_json::to_string(&names(tools)).map_err(|error| fail(name)(&error));
            let now = Timestamp::now().millis();
            sqlx::query(
                "INSERT INTO agent_grants (id, organization_id, workspace_id, agent_id, installation_id, tools, \
                 approval_required, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(GrantId::generate().uuid())
            .bind(organization.uuid())
            .bind(workspace.uuid())
            .bind(agent.id.uuid())
            .bind(installation_id.uuid())
            .bind(json(granted)?)
            .bind(json(approval)?)
            .bind(now)
            .bind(now)
            .execute(&pool)
            .await
            .map_err(|error| fail(name)(&error))?;
            pool.close().await;
            continue;
        }
        agents
            .create_grant(
                organization,
                workspace,
                agent.id,
                GrantId::generate(),
                installation_id,
                &GrantTools {
                    tools: names(granted),
                    approval_required: names(approval),
                },
                &|_: &Grant| {
                    by_fixture(
                        AuditAction::AgentGrantCreated,
                        AuditTarget::new(AuditObject::Agent(agent.id), &agent.name),
                        AuditPlace::Workspace(workspace),
                    )
                },
            )
            .await
            .map_err(|error| fail(name)(&error))?;
    }
    let (organization, workspace) = workspace_id("acme").map(|(workspace, _)| (northwind.id, workspace))?;
    let paused = agents
        .create_agent(
            organization,
            workspace,
            AgentId::generate(),
            &NewAgent {
                name: "Support concierge".to_owned(),
                description: "Not granted any installations yet.".to_owned(),
                model: "model-fast".to_owned(),
                instructions: "Answer support questions briefly.".to_owned(),
            },
            &|created: &Agent| {
                by_fixture(
                    AuditAction::AgentCreated,
                    AuditTarget::new(AuditObject::Agent(created.id), &created.name),
                    AuditPlace::Workspace(workspace),
                )
            },
        )
        .await
        .map_err(|error| fail("paused agent")(&error))?;
    agents
        .update_agent(
            organization,
            workspace,
            paused.id,
            &zupflow_core::agents::AgentChanges {
                status: Some(zupflow_core::agents::AgentStatus::Paused),
                ..Default::default()
            },
            &|agent: &Agent| {
                by_fixture(
                    AuditAction::AgentPaused,
                    AuditTarget::new(AuditObject::Agent(agent.id), &agent.name),
                    AuditPlace::Workspace(workspace),
                )
            },
        )
        .await
        .map_err(|error| fail("paused agent")(&error))?;

    // Runs, as the run engine leaves them: a finished one for the agent of
    // `acme`, `beta-labs`, `acme-sandbox`, `qa-playground` and `harbor`, and in `beta-labs` one
    // waiting for approval of `send_email`; `qa-playground` also waits for
    // `echo_write`, giving the shared suite a fixed pending call in each environment.
    // None is queued or running, so
    // nothing moves on its own (the contract server runs no worker).
    let runs = SqliteRunStore::new(database.clone());
    for (label, tool, waiting) in [
        ("Marketing", "search_files", false),
        ("Google", "search_files", false),
        ("Google", "send_email", true),
        ("Engineering", "list_tasks", false),
        ("Echo", "echo", false),
        ("Echo", "echo_write", true),
        ("Harbor", "list_tasks", false),
    ] {
        let (_, organization, workspace, installation_id, agent_id, model) = seeded_agents
            .iter()
            .find(|seeded| seeded.0 == label)
            .copied()
            .ok_or_else(|| format!("agent of {label}"))?;
        let installation = installations
            .find_installation(organization, workspace, installation_id)
            .await
            .map_err(|error| fail(label)(&error))?
            .ok_or_else(|| format!("installation {label}"))?;
        let app_name = match installation.app {
            InstalledApp::Catalog(id) => apps.find_organization_app(organization, id).await,
            InstalledApp::Workspace(id) => apps.find_workspace_app(organization, workspace, id).await,
        }
        .map_err(|error| fail(label)(&error))?
        .ok_or_else(|| format!("app of {label}"))?
        .name;
        let target = CallTarget {
            installation_id,
            installation_label: label.to_owned(),
            app_name,
            tool: tool.to_owned(),
        };
        fixture_runs::seed(&runs, organization, workspace, agent_id, model, target, waiting)
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
            .save_settings(
                organization,
                Environment::Sandbox,
                &return_urls,
                &origins,
                &|_: &ConnectSettings| {
                    by_fixture(
                        AuditAction::ConnectSettingsUpdated,
                        AuditTarget::connect_settings(Environment::Sandbox),
                        AuditPlace::Environment(Environment::Sandbox),
                    )
                },
            )
            .await
            .map_err(|error| fail("connect settings")(&error))?;
    }
    for (organization, workspace, slug) in [
        (northwind.id, "acme", "google-workspace"),
        (northwind.id, "beta-labs", "google-workspace"),
        (larkspur.id, "harbor", "taskboard"),
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

    // Endpoints in both Northwind environments and in Larkspur, with the
    // same frozen delivery scenarios as the mock. Secrets are sealed as in
    // the API; history adds no jobs, so live workers never advance it.
    let webhooks = SqliteWebhookStore::new(database.clone());
    let history_pool = SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(path)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5)),
    )
    .await
    .map_err(|error| fail("webhook history connection")(&error))?;
    for (organization, environment, url, workspace_slugs) in [
        (
            northwind.id,
            Environment::Production,
            "https://hooks.northwind.example/zupflow",
            &["acme", "beta-labs"][..],
        ),
        (
            northwind.id,
            Environment::Sandbox,
            "https://hooks.northwind.example/sandbox",
            &["acme-sandbox", "qa-playground"][..],
        ),
        (
            larkspur.id,
            Environment::Production,
            "https://hooks.larkspur.example/zupflow",
            &["harbor"][..],
        ),
    ] {
        let endpoint_id = WebhookEndpointId::generate();
        let secret = new_signing_secret(environment).ok_or("signing secret")?;
        let context = EncryptionContext::webhook_signing_secret(organization, environment, endpoint_id, SECRET_VERSION);
        let sealed = seal(&sealing, &context, secret.as_bytes())
            .await
            .map_err(|error| fail("seal")(&error))?;
        webhooks
            .create_endpoint(
                organization,
                endpoint_id,
                &NewEndpoint {
                    environment,
                    url: url.to_owned(),
                    description: "Run notifications".to_owned(),
                    events: vec![
                        WebhookEventType::RunCompleted,
                        WebhookEventType::RunFailed,
                        WebhookEventType::RunWaitingForApproval,
                    ],
                    secret: sealed,
                    secret_last4: secret[secret.len() - 4..].to_owned(),
                },
                Timestamp::now(),
                &|endpoint: &WebhookEndpoint| {
                    by_fixture(
                        AuditAction::WebhookCreated,
                        AuditTarget::new(AuditObject::Webhook(endpoint.id), &endpoint.url),
                        AuditPlace::Environment(endpoint.environment),
                    )
                },
            )
            .await
            .map_err(|error| fail("webhook")(&error))?;
        let workspaces = workspace_slugs
            .iter()
            .map(|slug| workspace_id(slug).map(|(id, _)| id))
            .collect::<Result<Vec<_>, _>>()?;
        fixture_webhooks::seed(
            &history_pool,
            organization,
            environment,
            endpoint_id,
            &workspaces,
            Timestamp::now(),
        )
        .await?;
    }
    history_pool.close().await;

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
        keys.create_api_key(organization, ApiKeyId::generate(), &key, &|key: &ApiKey| {
            by_fixture(
                AuditAction::ApiKeyCreated,
                AuditTarget::new(AuditObject::ApiKey(key.id), &key.name),
                AuditPlace::Environment(key.environment),
            )
        })
        .await
        .map_err(|error| fail("key")(&error))?;
    }
    Ok(())
}

/// The fixture is built by changes like any other, so each records its
/// audit event as it would in the API; Zupflow itself made them.
fn by_fixture(action: AuditAction, target: AuditTarget, place: AuditPlace) -> AuditEntry {
    AuditEntry::new(
        AuditActor::System {
            name: "Fixture".to_owned(),
        },
        action,
        target,
        place,
    )
}

/// A tool as the MCP client records it: canonical JSON and its SHA-256.
fn fixture_tool(name: &str, title: &str, description: &str, effect: ToolEffect) -> ListedTool {
    let definition: Value = json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": { "query": { "type": "string" } } },
        "annotations": {
            "readOnlyHint": effect == ToolEffect::Read,
            "destructiveHint": effect == ToolEffect::Destructive,
        },
    });
    ListedTool {
        name: name.to_owned(),
        revision: fixture_revision(&definition),
        effect,
    }
}

fn fixture_revision(definition: &Value) -> ToolRevision {
    let definition = zupflow_mcp::canonical(definition);
    let hash = Sha256::digest(definition.as_bytes()).into();
    ToolRevision { definition, hash }
}

/// Moves only this pending fixture session's deadline into the past. The
/// normal API and expiry worker still own its terminal transition and events.
/// This command never accepts or returns a possession reference or OAuth state.
async fn expire_connect(
    pool: &SqlitePool,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session_id: ConnectSessionId,
) -> Result<(), String> {
    let now = Timestamp::now().millis();
    let changed = sqlx::query(
        "UPDATE connect_sessions SET created_at = ?, expires_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? \
         AND status = 'pending' AND expires_at > ?",
    )
    .bind(now - zupflow_core::connect::SESSION_TTL_MS - 1)
    .bind(now - 1)
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session_id.uuid())
    .bind(now)
    .execute(pool)
    .await
    .map_err(|_| "can't expire fixture session".to_owned())?;
    if changed.rows_affected() != 1 {
        return Err("no pending fixture session in that scope".to_owned());
    }
    Ok(())
}

async fn expire_connect_command(path: &str, organization: &str, workspace: &str, session: &str) -> Result<(), String> {
    let organization_id = organization
        .parse::<OrganizationId>()
        .map_err(|_| "invalid organization ID".to_owned())?;
    let workspace_id = workspace
        .parse::<WorkspaceId>()
        .map_err(|_| "invalid workspace ID".to_owned())?;
    let session_id = session
        .parse::<ConnectSessionId>()
        .map_err(|_| "invalid Connect session ID".to_owned())?;
    // Refuse a typo instead of creating or migrating another database.
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePool::connect_with(options)
        .await
        .map_err(|_| "can't open existing fixture database".to_owned())?;
    let result = expire_connect(&pool, organization_id, workspace_id, session_id).await;
    pool.close().await;
    result
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["serve", database, options @ ..] => match serving(options) {
            Ok(serving) => serve(database, serving).await,
            Err(error) => Err(error),
        },
        ["reset", database] => reset(database).await,
        ["expire-connect", database, organization, workspace, session] => {
            expire_connect_command(database, organization, workspace, session).await
        }
        ["operations"] => {
            #[allow(clippy::print_stdout)]
            {
                println!("{}", zupflow_server::SERVED_OPERATIONS.join(","));
            }
            Ok(())
        }
        _ => Err(
            "usage: zupflow-contract-server serve <database> [--console <origin>] [--local-app <host>=<certificate.pem>]... | reset <database> | expire-connect <database> <organization> <workspace> <session> | operations"
                .to_owned(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("zupflow-contract-server: {message}");
            ExitCode::FAILURE
        }
    }
}
