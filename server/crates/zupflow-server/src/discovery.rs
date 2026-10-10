//! Tool discovery (decision 0038): when an installation connects, a job lists
//! its app's tools through the installation's credential and records them in
//! the tool registry, as that installation's own.
//!
//! Discovery is background work: the job carries the installation's
//! reference, never its credential, and the worker reads everything again in
//! the job's own scope. Delivery is at least once, and recording a listing is
//! idempotent. A connect never waits for, or fails because of, discovery.

use std::time::Duration;

use serde_json::{Value, json};
use zupflow_core::apps::AuthTemplate;
use zupflow_core::ids::{InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{InstallationStatus, InstalledApp};
use zupflow_core::jobs::{Job, JobOptions};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::tools::ToolError;
use zupflow_mcp::{McpCredential, McpError};

use crate::auth::AppState;

/// The job kind.
pub const DISCOVER_TOOLS: &str = "discover_tools";

/// How long a claim holds a discovery: longer than every call it makes.
const LEASE: Duration = Duration::from_secs(5 * 60);

/// How often an idle worker looks for work.
const IDLE: Duration = Duration::from_secs(1);

const ATTEMPTS: u32 = 5;

fn kind() -> Option<Name> {
    Name::new(DISCOVER_TOOLS).ok()
}

/// Asks for the installation's tools to be discovered. Best effort: a failure
/// is logged, and the installation is connected either way.
pub async fn enqueue(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: InstallationId,
) {
    let Some(kind) = kind() else { return };
    let payload = json!({ "installation_id": installation_id.to_string() }).to_string();
    let enqueued = state
        .jobs
        .enqueue(
            &Scope::Workspace(organization_id, workspace_id),
            &kind,
            payload.into_bytes(),
            JobOptions {
                delay: Duration::ZERO,
                max_attempts: ATTEMPTS,
            },
        )
        .await;
    if let Err(error) = enqueued {
        tracing::warn!(event = "discovery.enqueue_failed", %organization_id, %workspace_id, %installation_id, %error);
    }
}

/// What came of one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Done, whether or not anything was recorded: trying again wouldn't help.
    Done,
    /// Worth another attempt later, with why.
    Retry(&'static str),
}

/// Discovers one installation's tools, in its own scope.
pub async fn discover(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: InstallationId,
) -> Outcome {
    let skip = |why: &'static str| {
        tracing::info!(event = "discovery.skipped", %organization_id, %workspace_id, %installation_id, why);
        Outcome::Done
    };
    let installation = match state
        .installations
        .find_installation(organization_id, workspace_id, installation_id)
        .await
    {
        Ok(Some(installation)) => installation,
        Ok(None) => return skip("installation_gone"),
        Err(_) => return Outcome::Retry("store_unavailable"),
    };
    let (Some(connection), InstallationStatus::Connected) = (&installation.connection, installation.status) else {
        return skip("not_connected");
    };
    // The credential is used as the installation connected under it (decision 0036).
    let Some(snapshot) = &installation.snapshot else {
        return skip("no_template_snapshot");
    };
    let workspace = match state.workspaces.find_workspace(organization_id, workspace_id).await {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return skip("workspace_gone"),
        Err(_) => return Outcome::Retry("store_unavailable"),
    };
    let environment = workspace.environment;
    let app_id = installation.app.id();
    let configuration = match installation.app {
        InstalledApp::Catalog(app_id) => {
            state
                .apps
                .find_organization_configuration(organization_id, app_id, environment)
                .await
        }
        InstalledApp::Workspace(app_id) => {
            state
                .apps
                .find_workspace_configuration(organization_id, workspace_id, app_id)
                .await
        }
    };
    let server_url = match configuration {
        Ok(Some(configuration)) => match configuration.mcp_server_url {
            Some(url) => url,
            None => return skip("no_mcp_server"),
        },
        Ok(None) => return skip("not_configured"),
        Err(_) => return Outcome::Retry("store_unavailable"),
    };
    let sealed = match state
        .installations
        .find_credential(organization_id, workspace_id, installation_id)
        .await
    {
        Ok(Some(sealed)) => sealed,
        Ok(None) => return skip("no_credential"),
        Err(_) => return Outcome::Retry("store_unavailable"),
    };
    if sealed.version != connection.credential_version {
        // Reconnected since: that connect queued its own discovery.
        return skip("credential_replaced");
    }
    let context = EncryptionContext::installation_credential(
        organization_id,
        workspace_id,
        environment,
        app_id,
        installation_id,
        sealed.version,
    );
    let opened = match keys::open(state.keys.as_ref(), &context, &sealed.sealed).await {
        Ok(opened) => opened,
        Err(error) => {
            tracing::error!(event = "discovery.credential_unopenable", %organization_id, %workspace_id, %installation_id, %error);
            return Outcome::Retry("credential_unopenable");
        }
    };
    let values: serde_json::Map<String, Value> = match serde_json::from_slice(opened.expose()) {
        Ok(values) => values,
        Err(_) => {
            tracing::error!(event = "discovery.credential_unreadable", %organization_id, %workspace_id, %installation_id);
            return Outcome::Done;
        }
    };
    let text = |key: &str| values.get(key).and_then(Value::as_str);
    let (header, prefix, secret) = match &snapshot.auth {
        AuthTemplate::Token { inputs, placement, .. } => (
            placement.name.clone(),
            placement.prefix.as_str(),
            inputs.first().and_then(|input| text(&input.key)),
        ),
        AuthTemplate::OAuth2 { .. } => ("Authorization".to_owned(), "Bearer ", text("access_token")),
    };
    let Some(secret) = secret else {
        return skip("credential_without_token");
    };
    let value = zeroize::Zeroizing::new(format!("{prefix}{secret}"));
    let credential = McpCredential {
        header: &header,
        value: &value,
        secret,
    };
    let listed = match zupflow_mcp::list_tools(state.outbound.as_ref(), &server_url, Some(credential)).await {
        Ok(listed) => listed,
        Err(McpError::Unavailable) => return Outcome::Retry("server_unavailable"),
        Err(error) => {
            // The server's own words are never logged, only what was wrong.
            tracing::warn!(event = "discovery.listing_failed", %organization_id, %workspace_id, %installation_id, %error);
            return Outcome::Done;
        }
    };
    match state
        .tools
        .record_listing(
            organization_id,
            workspace_id,
            installation_id,
            &listed,
            Timestamp::now(),
        )
        .await
    {
        Ok(changes) => {
            tracing::info!(
                event = "discovery.recorded",
                %organization_id,
                %workspace_id,
                %installation_id,
                tools = listed.len(),
                added = changes.added.len(),
                changed = changes.changed.len(),
                gone = changes.gone.len(),
                returned = changes.returned.len(),
            );
            Outcome::Done
        }
        Err(ToolError::Unavailable) => Outcome::Retry("store_unavailable"),
        Err(error) => {
            tracing::warn!(event = "discovery.record_failed", %organization_id, %workspace_id, %installation_id, %error);
            Outcome::Done
        }
    }
}

/// Claims and handles one discovery job, if one is due. Whether one was.
pub async fn work_once(state: &AppState) -> bool {
    let Some(kind) = kind() else { return false };
    // Cross-tenant by design: a worker takes the next job of any scope.
    let job = match state.jobs.claim_next(std::slice::from_ref(&kind), LEASE).await {
        Ok(Some(job)) => job,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(event = "discovery.claim_failed", %error);
            return false;
        }
    };
    handle(state, job).await;
    true
}

async fn handle(state: &AppState, job: Job) {
    let Job {
        id,
        scope,
        payload,
        attempt,
        lease,
        ..
    } = job;
    let installation_id = serde_json::from_slice::<Value>(&payload)
        .ok()
        .and_then(|payload| payload.get("installation_id")?.as_str().map(str::to_owned))
        .and_then(|id| InstallationId::parse(&id).ok());
    let (Scope::Workspace(organization_id, workspace_id), Some(installation_id)) = (scope, installation_id) else {
        tracing::error!(event = "discovery.malformed_job", kind = DISCOVER_TOOLS);
        if let Ok(reason) = Name::new("malformed") {
            let _ = state.jobs.bury(&scope, id, &lease, &reason).await;
        }
        return;
    };
    let finished = match discover(state, organization_id, workspace_id, installation_id).await {
        Outcome::Done => state.jobs.complete(&scope, id, &lease).await.map(|_| ()),
        Outcome::Retry(reason) => match Name::new(reason) {
            Ok(reason) => {
                let after = Duration::from_secs(30 * u64::from(attempt.max(1)));
                state.jobs.retry(&scope, id, &lease, after, &reason).await.map(|_| ())
            }
            Err(_) => state.jobs.complete(&scope, id, &lease).await,
        },
    };
    if let Err(error) = finished {
        tracing::warn!(event = "discovery.finish_failed", %organization_id, %workspace_id, %installation_id, %error);
    }
}

/// Works discovery jobs until the process stops.
pub async fn run_worker(state: AppState) {
    loop {
        if !work_once(&state).await {
            tokio::time::sleep(IDLE).await;
        }
    }
}
