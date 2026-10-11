//! Reaching an installation's MCP server as the installation: the server URL
//! its app's configuration names in the workspace's environment, and the
//! credential the installation connected with, opened only here, for the
//! call about to be made (decision 0036). Tool discovery and the run engine
//! both reach a server this way.

use serde_json::Value;
use zeroize::Zeroizing;
use zupflow_core::apps::AuthTemplate;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::installations::{Installation, InstallationStatus, InstalledApp};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_mcp::McpCredential;

use crate::auth::AppState;

/// The server and the credential to reach it with.
pub struct Reach {
    pub server_url: String,
    version: u32,
    header: String,
    value: Zeroizing<String>,
    secret: Zeroizing<String>,
}

impl Reach {
    /// The credential as the MCP client sends it, injected last.
    pub fn credential(&self) -> McpCredential<'_> {
        McpCredential {
            header: &self.header,
            value: &self.value,
            secret: &self.secret,
        }
    }
}

/// Why a server can't be reached as the installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreachable {
    /// Not now, nor by trying again: why.
    Skip(&'static str),
    /// Worth trying again later: why.
    Retry(&'static str),
}

impl Unreachable {
    pub fn why(self) -> &'static str {
        match self {
            Self::Skip(why) | Self::Retry(why) => why,
        }
    }

    /// Whether the installation needs reconnecting before it can be used.
    pub fn needs_reconnect(self) -> bool {
        matches!(
            self,
            Self::Skip(
                "not_connected" | "no_credential" | "credential_without_token" | "server_moved" | "reconnect_required"
            )
        )
    }
}

/// The installation's server and credential, read again in its own scope.
async fn open_reach(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation: &Installation,
) -> Result<Reach, Unreachable> {
    let installation_id = installation.id;
    let (Some(connection), InstallationStatus::Connected) = (&installation.connection, installation.status) else {
        return Err(Unreachable::Skip("not_connected"));
    };
    // The credential is used as the installation connected under it (decision 0036).
    let Some(snapshot) = &installation.snapshot else {
        return Err(Unreachable::Skip("no_template_snapshot"));
    };
    let workspace = match state.workspaces.find_workspace(organization_id, workspace_id).await {
        Ok(Some(workspace)) => workspace,
        Ok(None) => return Err(Unreachable::Skip("workspace_gone")),
        Err(_) => return Err(Unreachable::Retry("store_unavailable")),
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
            None => return Err(Unreachable::Skip("no_mcp_server")),
        },
        Ok(None) => return Err(Unreachable::Skip("not_configured")),
        Err(_) => return Err(Unreachable::Retry("store_unavailable")),
    };
    // A token signed in through the MCP server was asked for that server
    // alone (RFC 8707): never sent to another the configuration names since.
    if let AuthTemplate::McpOAuth { .. } = snapshot.auth
        && snapshot
            .client
            .as_ref()
            .is_none_or(|client| client.resource != server_url)
    {
        return Err(Unreachable::Skip("server_moved"));
    }
    let sealed = match state
        .installations
        .find_credential(organization_id, workspace_id, installation_id)
        .await
    {
        Ok(Some(sealed)) => sealed,
        Ok(None) => return Err(Unreachable::Skip("no_credential")),
        Err(_) => return Err(Unreachable::Retry("store_unavailable")),
    };
    if sealed.version != connection.credential_version {
        // Reconnected since this installation was read: read it again.
        return Err(Unreachable::Retry("credential_replaced"));
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
            tracing::error!(event = "mcp.credential_unopenable", %organization_id, %workspace_id, %installation_id, %error);
            return Err(Unreachable::Retry("credential_unopenable"));
        }
    };
    let values: serde_json::Map<String, Value> = match serde_json::from_slice(opened.expose()) {
        Ok(values) => values,
        Err(_) => {
            tracing::error!(event = "mcp.credential_unreadable", %organization_id, %workspace_id, %installation_id);
            return Err(Unreachable::Skip("credential_unreadable"));
        }
    };
    let text = |key: &str| values.get(key).and_then(Value::as_str);
    let (header, prefix, secret) = match &snapshot.auth {
        AuthTemplate::Token { inputs, placement, .. } => (
            placement.name.clone(),
            placement.prefix.as_str(),
            inputs.first().and_then(|input| text(&input.key)),
        ),
        AuthTemplate::OAuth2 { .. } | AuthTemplate::McpOAuth { .. } => {
            ("Authorization".to_owned(), "Bearer ", text("access_token"))
        }
    };
    let Some(secret) = secret else {
        return Err(Unreachable::Skip("credential_without_token"));
    };
    Ok(Reach {
        server_url,
        version: sealed.version,
        header,
        value: Zeroizing::new(format!("{prefix}{secret}")),
        secret: Zeroizing::new(secret.to_owned()),
    })
}

/// Reads current state in the explicit scope, refreshing once before use
/// when expiry is at most 30 seconds away. A racing winner is read again.
pub async fn reach(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation: &Installation,
) -> Result<Reach, Unreachable> {
    let (current, reach) = current_reach(state, organization_id, workspace_id, installation.id).await?;
    let expires = current.connection.as_ref().and_then(|c| c.expires_at);
    if !matches!(
        current.snapshot.as_ref().map(|s| &s.auth),
        Some(AuthTemplate::Token { .. })
    ) && expires
        .is_some_and(|at| at.millis() <= zupflow_core::time::Timestamp::now().millis().saturating_add(30_000))
    {
        crate::oauth_refresh::refresh(state, organization_id, workspace_id, &current).await?;
        return current_reach(state, organization_id, workspace_id, current.id)
            .await
            .map(|(_, reach)| reach);
    }
    Ok(reach)
}

// A reconnect or refresh can commit between the two reads. Retry only
// this pre-send snapshot race, bounded so continuous changes can't starve.
async fn current_reach(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: zupflow_core::ids::InstallationId,
) -> Result<(Installation, Reach), Unreachable> {
    for _ in 0..3 {
        let installation = current(state, organization_id, workspace_id, installation_id).await?;
        match open_reach(state, organization_id, workspace_id, &installation).await {
            Err(Unreachable::Retry("credential_replaced")) => continue,
            result => return result.map(|reach| (installation, reach)),
        }
    }
    Err(Unreachable::Retry("credential_replaced"))
}

async fn current(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: zupflow_core::ids::InstallationId,
) -> Result<Installation, Unreachable> {
    state
        .installations
        .find_installation(organization_id, workspace_id, installation_id)
        .await
        .map_err(|_| Unreachable::Retry("store_unavailable"))?
        .ok_or(Unreachable::Skip("not_connected"))
}

async fn after_unauthorized(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: zupflow_core::ids::InstallationId,
    previous: &Reach,
) -> Result<Reach, Unreachable> {
    let (installation, current) = current_reach(state, organization_id, workspace_id, installation_id).await?;
    if current.version != previous.version {
        return Ok(current);
    }
    crate::oauth_refresh::refresh(state, organization_id, workspace_id, &installation).await?;
    current_reach(state, organization_id, workspace_id, installation_id)
        .await
        .map(|(_, reach)| reach)
}

/// Only a 401 proves the tool did not execute. No other result is replayed.
pub async fn call_tool(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: zupflow_core::ids::InstallationId,
    reach: Reach,
    name: &str,
    arguments: &serde_json::Map<String, Value>,
) -> Result<zupflow_mcp::ToolResult, zupflow_mcp::CallError> {
    use zupflow_mcp::{CallError, McpError};
    let first = zupflow_mcp::call_tool(
        state.outbound.as_ref(),
        &reach.server_url,
        Some(reach.credential()),
        name,
        arguments,
        zupflow_core::outbound::Limits::default(),
    )
    .await;
    if first != Err(CallError::Unauthorized) {
        return first;
    }
    let renewed = after_unauthorized(state, organization_id, workspace_id, installation_id, &reach)
        .await
        .map_err(|error| {
            if error.needs_reconnect() {
                CallError::Unauthorized
            } else {
                CallError::NotSent(McpError::Unavailable)
            }
        })?;
    let second = zupflow_mcp::call_tool(
        state.outbound.as_ref(),
        &renewed.server_url,
        Some(renewed.credential()),
        name,
        arguments,
        zupflow_core::outbound::Limits::default(),
    )
    .await;
    if second == Err(CallError::Unauthorized) {
        crate::oauth_refresh::require_reconnect(state, organization_id, workspace_id, installation_id, renewed.version)
            .await
            .map_err(|_| CallError::NotSent(McpError::Unavailable))?;
    }
    second
}

pub async fn list_tools(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: zupflow_core::ids::InstallationId,
    reach: Reach,
) -> Result<Vec<zupflow_core::tools::ListedTool>, zupflow_mcp::McpError> {
    use zupflow_mcp::McpError;
    let first = zupflow_mcp::list_tools(state.outbound.as_ref(), &reach.server_url, Some(reach.credential())).await;
    if first != Err(McpError::Unauthorized) {
        return first;
    }
    let renewed = after_unauthorized(state, organization_id, workspace_id, installation_id, &reach)
        .await
        .map_err(|error| {
            if error.needs_reconnect() {
                McpError::Unauthorized
            } else {
                McpError::Unavailable
            }
        })?;
    let second =
        zupflow_mcp::list_tools(state.outbound.as_ref(), &renewed.server_url, Some(renewed.credential())).await;
    if second == Err(McpError::Unauthorized) {
        crate::oauth_refresh::require_reconnect(state, organization_id, workspace_id, installation_id, renewed.version)
            .await
            .map_err(|_| McpError::Unavailable)?;
    }
    second
}
