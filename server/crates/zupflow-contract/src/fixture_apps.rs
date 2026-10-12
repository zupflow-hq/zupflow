//! The reference fixture's apps, as the contract suite's `Target` describes
//! them: the same definitions the Console's mock seeds.

use serde_json::{Value, json};
use zupflow_core::apps::{AppIcon, AppKind, ConfigurationChanges, NewApp, NewClientSecret};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{ConnectedAccount, InstalledApp, NewCredential, NewInstallation, TemplateSnapshot};
use zupflow_core::keys::{self, EncryptionContext, KeyService};

/// A configuration to seed: a server, and for an OAuth app a client.
pub struct Ready {
    pub mcp_server_url: String,
    /// With a client ID and a sealed client secret, so an OAuth app is ready.
    pub oauth: bool,
}

impl Ready {
    /// The write, sealing the client secret under its owner's context as the API does.
    pub async fn changes(
        &self,
        keys: &dyn KeyService,
        organization_id: OrganizationId,
        workspace_id: Option<WorkspaceId>,
        environment: Environment,
        app_id: AppId,
    ) -> Result<ConfigurationChanges, String> {
        let client_secret = if self.oauth {
            let context = EncryptionContext::oauth_client_secret(organization_id, workspace_id, environment, app_id, 1);
            let sealed = keys::seal(keys, &context, b"fixture-client-secret")
                .await
                .map_err(|error| error.to_string())?;
            Some(NewClientSecret {
                sealed,
                last4: "cret".to_owned(),
                version: 1,
            })
        } else {
            None
        };
        Ok(ConfigurationChanges {
            mcp_server_url: Some(Some(self.mcp_server_url.clone())),
            oauth_client_id: self.oauth.then(|| Some("fixture-client".to_owned())),
            client_secret,
            oauth_scopes: None,
            updated_by: None,
        })
    }
}

/// An app from its contract-shaped JSON.
fn app(definition: Value) -> Result<NewApp, String> {
    let text = |field: &str| {
        definition[field]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("fixture app without {field}"))
    };
    Ok(NewApp {
        kind: AppKind::Mcp,
        name: text("name")?,
        key: text("key")?,
        description: text("description")?,
        icon: AppIcon {
            glyph: definition["icon"][0].as_str().unwrap_or_default().to_owned(),
            color: definition["icon"][1].as_str().unwrap_or_default().to_owned(),
        },
        auth: serde_json::from_value(definition["auth"].clone()).map_err(|error| error.to_string())?,
        tools: serde_json::from_value(definition["tools"].clone()).map_err(|error| error.to_string())?,
    })
}

fn tool(name: &str, title: &str, description: &str, effect: &str) -> Value {
    json!({ "name": name, "title": title, "description": description, "effect": effect })
}

fn scope(name: &str, description: &str) -> Value {
    json!({ "name": name, "description": description })
}

fn token_auth(label: &str, help: &str, placeholder: &str, verify_url: &str, instructions: &str) -> Value {
    json!({
        "type": "token",
        "inputs": [{
            "key": "token", "label": label, "secret": true,
            "help": help, "placeholder": placeholder, "pattern": null,
        }],
        "placement": { "location": "header", "name": "Authorization", "prefix": "Bearer " },
        "verify_url": verify_url,
        "instructions": instructions,
        "docs_url": null,
    })
}

pub fn google_workspace() -> Result<NewApp, String> {
    app(json!({
        "name": "Google Workspace",
        "key": "google_workspace",
        "description": "Drive, Docs, Gmail and Calendar for the connected Google account.",
        "icon": ["G", "#3B6FD8"],
        "auth": {
            "type": "oauth2",
            "authorize_url": "https://accounts.google.com/o/oauth2/v2/auth",
            "token_url": "https://oauth2.googleapis.com/token",
            "pkce": true,
            "identity_url": "https://openidconnect.googleapis.com/v1/userinfo",
            "available_scopes": [
                scope("openid", "Identify the connected account"),
                scope("email", "Read the account's email address"),
                scope("https://www.googleapis.com/auth/drive.file", "Files the app creates or the user opens with it"),
                scope("https://www.googleapis.com/auth/drive.readonly", "Read all Drive files"),
                scope("https://www.googleapis.com/auth/gmail.send", "Send email"),
                scope("https://www.googleapis.com/auth/calendar.events", "Read and edit calendar events"),
            ],
        },
        "tools": [
            tool("search_files", "Search files", "Find Drive files by name or content.", "read"),
            tool("read_document", "Read document", "Return the text of a Google Doc.", "read"),
            tool("create_document", "Create document", "Create a Google Doc in a chosen folder.", "write"),
            tool("list_events", "List calendar events", "List events in a time range.", "read"),
            tool("send_email", "Send email", "Send an email from the connected account.", "write"),
            tool("delete_file", "Delete file", "Move a Drive file to the trash.", "destructive"),
        ],
    }))
}

/// Static configured-OAuth fixture; live journeys create a separate MCP OAuth app.
pub fn taskboard() -> Result<NewApp, String> {
    app(json!({
        "name": "Taskboard",
        "key": "taskboard",
        "description": "Tasks on the connected board, for development and testing.",
        "icon": ["T", "#0E7490"],
        "auth": {
            "type": "oauth2",
            "authorize_url": "https://taskboard.example/authorize",
            "token_url": "https://taskboard.example/token",
            "issuer": "https://taskboard.example",
            "pkce": true,
            "identity_url": "https://taskboard.example/me",
            "available_scopes": [
                scope("tasks:read", "See the tasks on your board"),
                scope("tasks:write", "Create and delete tasks on your board"),
            ],
        },
        "tools": [
            tool(
                "list_tasks",
                "List tasks",
                "Lists the tasks on the board, oldest first. Optionally only open or done ones.",
                "read",
            ),
            tool("create_task", "Create a task", "Adds an open task to the board.", "write"),
            tool("delete_task", "Delete a task", "Deletes a task from the board for good.", "destructive"),
        ],
    }))
}

pub fn notion() -> Result<NewApp, String> {
    app(json!({
        "name": "Notion",
        "key": "notion",
        "description": "Pages and databases shared with the integration.",
        "icon": ["N", "#2F3437"],
        "auth": {
            "type": "oauth2",
            "authorize_url": "https://api.notion.com/v1/oauth/authorize",
            "token_url": "https://api.notion.com/v1/oauth/token",
            "pkce": false,
            "identity_url": "https://api.notion.com/v1/users/me",
            "available_scopes": [],
        },
        "tools": [
            tool("search_pages", "Search pages", "Find pages shared with the integration.", "read"),
            tool("append_blocks", "Append to page", "Add content to the end of a page.", "write"),
        ],
    }))
}

/// A workspace app: Acme's own CMS, or Harbor's.
pub fn publishing(name: &str, glyph: &str, key: &str, host: &str) -> Result<NewApp, String> {
    app(json!({
        "name": name,
        "key": key,
        "description": format!("{name}'s own CMS. Drafts, schedules and publishes blog posts."),
        "icon": [glyph, "#C2410C"],
        "auth": token_auth(
            "CMS token",
            "Settings → Integrations → Personal tokens in the CMS.",
            "cms_…",
            &format!("https://{host}/api/me"),
            "Paste a token from the CMS account the agent should publish as.",
        ),
        "tools": [
            tool("list_drafts", "List drafts", "List unpublished posts.", "read"),
            tool("create_draft", "Create draft", "Create a post draft from Markdown.", "write"),
            tool("publish_post", "Publish post", "Publish a draft to the public blog.", "destructive"),
        ],
    }))
}

pub fn echo_server() -> Result<NewApp, String> {
    app(json!({
        "name": "Echo test server",
        "key": "echo_server",
        "description": "Returns its input. For exercising grants and approvals in Sandbox.",
        "icon": ["E", "#0E7490"],
        "auth": token_auth(
            "Token",
            "Any value. The echo server accepts every token except `invalid`.",
            "test-token",
            "https://echo.example/me",
            "Use any token to connect.",
        ),
        "tools": [
            tool("echo", "Echo", "Return the input unchanged.", "read"),
            tool("echo_write", "Echo as write", "Return the input, marked as a write.", "write"),
        ],
    }))
}

/// Where an installation is, for the context its credential is sealed under.
pub struct Owner {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub environment: Environment,
    pub installation_id: InstallationId,
}

/// A connected installation whose credential is sealed under its own
/// `installation_credential` context, version 1.
pub async fn installation(
    keys: &dyn KeyService,
    snapshot: TemplateSnapshot,
    owner: Owner,
    app: InstalledApp,
    label: &str,
) -> Result<NewInstallation, String> {
    let context = EncryptionContext::installation_credential(
        owner.organization_id,
        owner.workspace_id,
        owner.environment,
        app.id(),
        owner.installation_id,
        1,
    );
    let sealed = keys::seal(keys, &context, br#"{"token":"fixture-token"}"#)
        .await
        .map_err(|error| error.to_string())?;
    let handle = label.to_ascii_lowercase();
    Ok(NewInstallation {
        app,
        label: label.to_owned(),
        configuration_version: 1,
        account: ConnectedAccount {
            external_id: format!("acct-{handle}"),
            display_name: format!("{label} account"),
            email: Some(format!("{handle}@accounts.example")),
        },
        granted_scopes: Vec::new(),
        expires_at: None,
        credential: NewCredential { sealed, version: 1 },
        snapshot,
    })
}
