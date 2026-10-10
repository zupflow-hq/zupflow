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
mod fixture_runs;

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
use zupflow_core::agents::{AgentStore, GrantTools, NewAgent};
use zupflow_core::api_keys::{ApiKeyHasher, ApiKeyStore, NewApiKey, NewSecret};
use zupflow_core::apps::AppStore;
use zupflow_core::connect::{ConnectIntent, ConnectStore, NewConnectSession, Reference};
use zupflow_core::environment::Environment;
use zupflow_core::idempotency::Fingerprinter;
use zupflow_core::identity::{Identity, IdentityError, Profile, VerifiedIdentity};
use zupflow_core::ids::{
    AgentId, ApiKeyId, AppId, ConnectSessionId, GrantId, InstallationId, OrganizationId, WorkspaceId,
};
use zupflow_core::installations::{InstallationStore, InstalledApp, TemplateSnapshot};
use zupflow_core::organizations::{MemberRole, OrganizationStore, User};
use zupflow_core::outbound::{Limits, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse, Refusal};
use zupflow_core::policy::{ToolLevel, ToolPolicyStore};
use zupflow_core::runs::CallTarget;
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{ListedTool, ToolRegistry, ToolRevision};
use zupflow_core::workspaces::{NewWorkspace, WorkspaceStore};
use zupflow_server::{AppState, RequestGuard};
use zupflow_sqlite::{
    Database, SqliteAgentStore, SqliteApiKeyStore, SqliteAppStore, SqliteConnectStore, SqliteIdempotencyStore,
    SqliteInstallationStore, SqliteOrganizationStore, SqliteRunStore, SqliteToolPolicyStore, SqliteToolRegistry,
    SqliteWorkspaceStore,
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
        agents: Arc::new(SqliteAgentStore::new(database.clone())),
        policies: Arc::new(zupflow_sqlite::SqliteToolPolicyStore::new(database.clone())),
        runs: Arc::new(zupflow_sqlite::SqliteRunStore::new(database.clone())),
        provider: Arc::new(zupflow_basic::no_model::NoModelProvider),
        approvals: Arc::new(zupflow_basic::configured_policy::ConfiguredPolicy::new(Arc::new(
            zupflow_sqlite::SqliteToolPolicyStore::new(database.clone()),
        ))),
        events: Arc::new(zupflow_basic::in_process_events::InProcessEventBus::default()),
        models: Arc::new(ConfiguredModels::parse(MODELS).map_err(|error| error.to_string())?),
        connect: Arc::new(SqliteConnectStore::new(database.clone())),
        jobs: Arc::new(zupflow_sqlite::SqliteJobQueue::new(database.clone())),
        tools: Arc::new(zupflow_sqlite::SqliteToolRegistry::new(database.clone())),
        cache: Arc::new(zupflow_basic::memory_cache::MemoryCache::default()),
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
            .map(|tool| ListedTool {
                effect: tool.effect,
                ..fixture_tool(
                    &tool.name,
                    &tool.title,
                    &tool.description,
                    tool.effect == zupflow_core::apps::ToolEffect::Read,
                )
            })
            .collect();
        tools
            .record_listing(organization, workspace, installation_id, &listing, Timestamp::now())
            .await
            .map_err(|error| fail("tools")(&error))?;
        suggest(organization, app, &listing).await?;
    }

    // The Sandbox's installation's tools, as two later listings left them:
    // (each listing suggests levels too, as discovery does)
    // one tool as first listed, one changed since, one gone.
    let (organization, workspace, installation_id, _) = installed
        .iter()
        .find(|(_, _, _, label)| *label == "Engineering")
        .copied()
        .ok_or("installation Engineering")?;
    for (listing, at) in [
        (
            vec![
                fixture_tool("search_issues", "Search issues", "Find issues by text.", true),
                fixture_tool("create_issue", "Create an issue", "Open an issue in a team.", false),
                fixture_tool("archive_issue", "Archive an issue", "Archive one issue.", false),
            ],
            Timestamp::from_millis(Timestamp::now().millis() - 86_400_000),
        ),
        (
            vec![
                fixture_tool("search_issues", "Search issues", "Find issues by text.", true),
                fixture_tool(
                    "create_issue",
                    "Create an issue",
                    "Open an issue in a team, assigned to anyone.",
                    false,
                ),
            ],
            Timestamp::now(),
        ),
    ] {
        tools
            .record_listing(organization, workspace, installation_id, &listing, at)
            .await
            .map_err(|error| fail("tools")(&error))?;
        let linear = seeded
            .iter()
            .find(|(owner, seeded, _)| *owner == organization && *seeded == "linear")
            .map(|(_, _, app)| *app)
            .ok_or("app linear")?;
        suggest(organization, linear, &listing).await?;
    }

    // The organizations' confirmed levels, as people set them: every tool
    // `acme`'s grants hold stays inside them; `list_events` stays suggested.
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
            "linear",
            owner.id,
            &[
                ("search_issues", ToolLevel::Allow),
                ("create_issue", ToolLevel::Allow),
                ("add_comment", ToolLevel::Allow),
            ],
        ),
        (
            larkspur.id,
            "linear",
            outsider.id,
            &[("search_issues", ToolLevel::Allow), ("create_issue", ToolLevel::Allow)],
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
            .set_levels(organization, app, &levels, Some(writer), Timestamp::now())
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
            &["search_issues", "create_issue"],
            &[],
        ),
        (
            "Echo",
            "Echo bot",
            "model-fast",
            &["echo", "echo_write"],
            &["echo_write"],
        ),
        ("Harbor", "Harbor assistant", "model-balanced", &["search_issues"], &[]),
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
        )
        .await
        .map_err(|error| fail("paused agent")(&error))?;

    // Runs, as the run engine leaves them: a finished one for the agent of
    // `acme`, `beta-labs`, `acme-sandbox` and `harbor`, and in `beta-labs` one
    // waiting for approval of `send_email`. None is queued or running, so
    // nothing moves on its own (the contract server runs no worker).
    let runs = SqliteRunStore::new(database.clone());
    for (label, tool, waiting) in [
        ("Marketing", "search_files", false),
        ("Google", "search_files", false),
        ("Google", "send_email", true),
        ("Engineering", "search_issues", false),
        ("Harbor", "search_issues", false),
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

/// A tool as the MCP client records it: canonical JSON and its SHA-256.
fn fixture_tool(name: &str, title: &str, description: &str, read_only: bool) -> ListedTool {
    let definition: Value = json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": { "query": { "type": "string" } } },
        "annotations": { "readOnlyHint": read_only, "destructiveHint": false },
    });
    let definition = zupflow_mcp::canonical(&definition);
    let hash = Sha256::digest(definition.as_bytes()).into();
    ListedTool {
        name: name.to_owned(),
        revision: ToolRevision { definition, hash },
        effect: if read_only {
            zupflow_core::apps::ToolEffect::Read
        } else {
            zupflow_core::apps::ToolEffect::Write
        },
    }
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
