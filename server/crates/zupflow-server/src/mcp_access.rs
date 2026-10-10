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
            Self::Skip("not_connected" | "no_credential" | "credential_without_token" | "server_moved")
        )
    }
}

/// The installation's server and credential, read again in its own scope.
pub async fn reach(
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
        header,
        value: Zeroizing::new(format!("{prefix}{secret}")),
        secret: Zeroizing::new(secret.to_owned()),
    })
}
