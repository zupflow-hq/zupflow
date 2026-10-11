//! Apps: MCP servers an organization offers in its catalog, or one workspace
//! keeps for itself. The two live apart (decision 0031): a catalog app belongs
//! to the organization, a workspace app to one workspace, and every
//! [`AppStore`] method names the organization first, then the workspace for a
//! workspace app.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::audit::Audit;
use crate::environment::Environment;
use crate::ids::{AppId, OrganizationId, UserId, WorkspaceId};
use crate::keys::Sealed;
use crate::organizations::User;
use crate::time::Timestamp;

/// Only `mcp` exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    Mcp,
}

impl AppKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        (value == "mcp").then_some(Self::Mcp)
    }
}

/// The tile an app is drawn with: one or two characters on a color.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppIcon {
    pub glyph: String,
    /// `#RRGGBB`.
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthScope {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenInput {
    pub key: String,
    pub label: String,
    pub secret: bool,
    pub help: Option<String>,
    pub placeholder: Option<String>,
    /// A regular expression the value must match.
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenLocation {
    Header,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenPlacement {
    pub location: TokenLocation,
    pub name: String,
    pub prefix: String,
}

/// How an app's users authenticate with its provider: the contract's
/// `AuthTemplate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthTemplate {
    #[serde(rename = "oauth2")]
    OAuth2 {
        authorize_url: Option<String>,
        token_url: Option<String>,
        available_scopes: Vec<OAuthScope>,
        pkce: bool,
        identity_url: Option<String>,
    },
    /// Sign-in through the MCP server (decision 0039): its authorization
    /// server is discovered at each sign-in, and a client registered with
    /// it once per organization, app and environment.
    #[serde(rename = "mcp_oauth")]
    McpOAuth {
        /// What sign-in asks for; none asks for the server's default.
        scopes: Vec<OAuthScope>,
        identity_url: Option<String>,
    },
    Token {
        inputs: Vec<TokenInput>,
        placement: TokenPlacement,
        verify_url: Option<String>,
        instructions: String,
        docs_url: Option<String>,
    },
}

impl AuthTemplate {
    /// The contract's `AuthType` for this template.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::OAuth2 { .. } => "oauth2",
            Self::McpOAuth { .. } => "mcp_oauth",
            Self::Token { .. } => "token",
        }
    }
}

/// Which kind of [`AuthTemplate`] a new app starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    OAuth2,
    McpOAuth,
    Token,
}

impl AuthType {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "oauth2" => Some(Self::OAuth2),
            "mcp_oauth" => Some(Self::McpOAuth),
            "token" => Some(Self::Token),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    Read,
    Write,
    Destructive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub title: String,
    pub description: String,
    pub effect: ToolEffect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: AppId,
    pub organization_id: OrganizationId,
    /// `None` for a catalog app; the owning workspace for a workspace app.
    pub owner_workspace_id: Option<WorkspaceId>,
    pub kind: AppKind,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub icon: AppIcon,
    pub auth: AuthTemplate,
    pub tools: Vec<ToolDefinition>,
    pub version: u32,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A new app's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApp {
    pub kind: AppKind,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub icon: AppIcon,
    pub auth: AuthTemplate,
    pub tools: Vec<ToolDefinition>,
}

/// The colors a new app's tile is drawn from, picked by its slug.
const ICON_COLORS: [&str; 5] = ["#0E7490", "#7C3AED", "#B45309", "#15803D", "#BE123C"];

impl NewApp {
    /// An app as the API creates it: a tile from its name and slug, an empty
    /// template of its authentication type, and no tools until its server is
    /// asked for them.
    pub fn blank(name: String, slug: String, description: String, auth_type: AuthType) -> Self {
        let glyph: String = name
            .chars()
            .next()
            .map(char::to_uppercase)
            .into_iter()
            .flatten()
            .collect();
        // FNV-1a, so a slug gets the same color the mock gives it.
        let hash = slug.bytes().fold(2_166_136_261_u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
        });
        let color = ICON_COLORS[(hash % 5) as usize].to_owned();
        let auth = match auth_type {
            AuthType::OAuth2 => AuthTemplate::OAuth2 {
                authorize_url: None,
                token_url: None,
                available_scopes: Vec::new(),
                pkce: true,
                identity_url: None,
            },
            AuthType::McpOAuth => AuthTemplate::McpOAuth {
                scopes: Vec::new(),
                identity_url: None,
            },
            AuthType::Token => AuthTemplate::Token {
                inputs: vec![TokenInput {
                    key: "token".to_owned(),
                    label: "API token".to_owned(),
                    secret: true,
                    help: None,
                    placeholder: None,
                    pattern: None,
                }],
                placement: TokenPlacement {
                    location: TokenLocation::Header,
                    name: "Authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                },
                verify_url: None,
                instructions: format!("Paste an API token for {name}."),
                docs_url: None,
            },
        };
        Self {
            kind: AppKind::Mcp,
            name,
            slug,
            description,
            icon: AppIcon { glyph, color },
            auth,
            tools: Vec::new(),
        }
    }
}

/// What an update changes; `None` keeps the value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppChanges<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    /// A whole new template, already checked; it raises the app's version.
    pub auth: Option<&'a AuthTemplate>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AppError {
    #[error("the store is unavailable")]
    Unavailable,
    /// Another app of the same owner (the catalog, or one workspace) uses the slug.
    #[error("that slug is taken")]
    SlugTaken,
    #[error("not found")]
    NotFound,
    /// A secret was written under a version another write already took.
    #[error("the client secret changed meanwhile")]
    SecretVersionTaken,
    /// A new template drops a scope this configuration still requests.
    /// Checked in the same transaction as the write.
    #[error("{scope} is still requested by the {environment:?} configuration")]
    ScopeInUse { environment: Environment, scope: String },
    /// A configuration requests a scope its app's template doesn't offer.
    /// Checked in the same transaction as the write.
    #[error("{0} is not offered by this app")]
    UnknownScope(String),
}

impl AuthTemplate {
    /// For an OAuth app, the first of `requested` it doesn't offer; a token
    /// app offers no scopes and refuses none (its configuration's scopes are
    /// unused). An `mcp_oauth` app's sign-in asks for its template's own
    /// scopes, so its configurations request none.
    pub fn first_unoffered<'a>(&self, requested: &'a [String]) -> Option<&'a String> {
        match self {
            Self::OAuth2 { available_scopes, .. } => requested
                .iter()
                .find(|scope| !available_scopes.iter().any(|offered| &offered.name == *scope)),
            Self::McpOAuth { .. } => requested.first(),
            Self::Token { .. } => None,
        }
    }
}

/// What the API may show of a stored client secret: never the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretState {
    pub last4: String,
    pub updated_at: Timestamp,
    /// Raised each time the secret changes; bound into its encryption context.
    pub version: u32,
}

/// An app's client configuration in one environment (a workspace app's is
/// its workspace's). A configuration never written reads as
/// [`Configuration::empty`], version 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configuration {
    pub app_id: AppId,
    pub environment: Environment,
    /// Raised by every write.
    pub version: u32,
    pub mcp_server_url: Option<String>,
    pub oauth_client_id: Option<String>,
    pub client_secret: Option<SecretState>,
    /// The last secret version written here, whether or not a secret is set
    /// now: the next secret takes this plus one.
    pub secret_version: u32,
    pub oauth_scopes: Vec<String>,
    pub updated_at: Option<Timestamp>,
    /// The member who last wrote it; `None` when an API key did, or the user is gone.
    pub updated_by: Option<User>,
}

impl Configuration {
    pub fn empty(app_id: AppId, environment: Environment) -> Self {
        Self {
            app_id,
            environment,
            version: 0,
            mcp_server_url: None,
            oauth_client_id: None,
            client_secret: None,
            secret_version: 0,
            oauth_scopes: Vec::new(),
            updated_at: None,
            updated_by: None,
        }
    }

    /// The fields still required before the app can be connected: the MCP
    /// server, and an OAuth app's client ID and secret. An `mcp_oauth` app
    /// registers its own client, so it needs only the server.
    pub fn missing(&self, auth: &AuthTemplate) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.mcp_server_url.is_none() {
            missing.push("mcp_server_url");
        }
        if matches!(auth, AuthTemplate::OAuth2 { .. }) {
            if self.oauth_client_id.is_none() {
                missing.push("oauth_client_id");
            }
            if self.client_secret.is_none() {
                missing.push("oauth_client_secret");
            }
        }
        missing
    }

    /// `unconfigured` with every required field missing, `ready` with none,
    /// `incomplete` between.
    pub fn status(&self, auth: &AuthTemplate) -> ConfigurationStatus {
        let required = if matches!(auth, AuthTemplate::OAuth2 { .. }) {
            3
        } else {
            1
        };
        match self.missing(auth).len() {
            0 => ConfigurationStatus::Ready,
            missing if missing == required => ConfigurationStatus::Unconfigured,
            _ => ConfigurationStatus::Incomplete,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationStatus {
    Unconfigured,
    Incomplete,
    Ready,
}

/// A new client secret, already sealed under the context of `version`, which
/// must be the stored secret's version plus one (1 for the first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClientSecret {
    pub sealed: Sealed,
    pub last4: String,
    pub version: u32,
}

/// What a write changes; `None` keeps the value, `Some(None)` clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigurationChanges {
    pub mcp_server_url: Option<Option<String>>,
    pub oauth_client_id: Option<Option<String>>,
    pub client_secret: Option<NewClientSecret>,
    pub oauth_scopes: Option<Vec<String>>,
    /// The member writing; `None` for an API key.
    pub updated_by: Option<UserId>,
}

/// A stored client secret, sealed, with the version it was sealed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredClientSecret {
    pub sealed: Sealed,
    pub version: u32,
}

/// An authorization server, as discovery read and checked it (decision
/// 0039): its `issuer` equals the URL its metadata was read for, and every
/// endpoint is HTTPS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationServer {
    pub issuer: String,
    #[serde(default)]
    pub response_iss_supported: bool,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
}

/// How a registered client authenticates at the token endpoint: as a public
/// client relying on PKCE, or with HTTP Basic when the server issued it a
/// secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuthentication {
    None,
    ClientSecretBasic,
}

impl ClientAuthentication {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ClientSecretBasic => "client_secret_basic",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "client_secret_basic" => Some(Self::ClientSecretBasic),
            _ => None,
        }
    }
}

/// A client registration's row, by which what it was issued is sealed.
/// Never shown outside the server, so it has no prefixed form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientRegistrationId(uuid::Uuid);

impl ClientRegistrationId {
    pub fn generate() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// For row mapping only.
    pub fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }

    pub fn uuid(self) -> uuid::Uuid {
        self.0
    }
}

/// A client Zupflow registered with an MCP server's authorization server
/// (RFC 7591), for one catalog app in one environment or one workspace app.
/// What the server issued beside the client ID (a client secret, a
/// registration access token) is sealed together as one value, under the
/// `oauth_client_registration` context of [`ClientRegistration::id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRegistration {
    pub id: ClientRegistrationId,
    pub server: AuthorizationServer,
    pub client_id: String,
    pub authentication: ClientAuthentication,
    /// `None` when the server issued neither a secret nor a registration access token.
    pub sealed: Option<Sealed>,
    pub created_at: Timestamp,
}

/// What [`AppStore::save_organization_client_registration`] stores. Sealed
/// under the context of `id`, before the store sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClientRegistration {
    pub id: ClientRegistrationId,
    pub server: AuthorizationServer,
    pub client_id: String,
    pub authentication: ClientAuthentication,
    pub sealed: Option<Sealed>,
}

/// Creating or changing an app or its configuration records its [`Audit`]
/// in the same transaction; keeping a registered client doesn't.
#[async_trait]
pub trait AppStore: Send + Sync {
    /// The organization's catalog, by name.
    async fn list_organization_apps(&self, organization_id: OrganizationId) -> Result<Vec<App>, AppError>;

    async fn create_organization_app(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        app: &NewApp,
        audit: &Audit<'_, App>,
    ) -> Result<App, AppError>;

    /// A catalog app, in this organization only; never a workspace app.
    async fn find_organization_app(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
    ) -> Result<Option<App>, AppError>;

    async fn update_organization_app(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        changes: AppChanges<'_>,
        audit: &Audit<'_, App>,
    ) -> Result<App, AppError>;

    /// The workspace's own apps, by name.
    async fn list_workspace_apps(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<App>, AppError>;

    /// `NotFound` unless the workspace is in the organization.
    async fn create_workspace_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
        app: &NewApp,
        audit: &Audit<'_, App>,
    ) -> Result<App, AppError>;

    /// A workspace app, in this workspace of this organization only.
    async fn find_workspace_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
    ) -> Result<Option<App>, AppError>;

    async fn update_workspace_app(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
        changes: AppChanges<'_>,
        audit: &Audit<'_, App>,
    ) -> Result<App, AppError>;

    /// Every written configuration of the organization's catalog apps.
    async fn list_organization_configurations(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<Configuration>, AppError>;

    /// A catalog app's configuration in one environment, if ever written.
    async fn find_organization_configuration(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Environment,
    ) -> Result<Option<Configuration>, AppError>;

    /// Writes a catalog app's configuration, creating it on first write, and
    /// raises its version. `NotFound` unless the app is in the organization's
    /// catalog; `SecretVersionTaken` if another write changed the secret first.
    async fn save_organization_configuration(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Environment,
        changes: &ConfigurationChanges,
        audit: &Audit<'_, Configuration>,
    ) -> Result<Configuration, AppError>;

    async fn find_organization_client_secret(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Environment,
    ) -> Result<Option<StoredClientSecret>, AppError>;

    /// Every written configuration of the workspace's own apps.
    async fn list_workspace_configurations(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Configuration>, AppError>;

    /// A workspace app's configuration, in its workspace's environment.
    async fn find_workspace_configuration(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
    ) -> Result<Option<Configuration>, AppError>;

    /// As [`AppStore::save_organization_configuration`], for a workspace app.
    async fn save_workspace_configuration(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
        changes: &ConfigurationChanges,
        audit: &Audit<'_, Configuration>,
    ) -> Result<Configuration, AppError>;

    async fn find_workspace_client_secret(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
    ) -> Result<Option<StoredClientSecret>, AppError>;

    /// The client registered for a catalog app in one environment at
    /// `issuer`, if any.
    async fn find_organization_client_registration(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Environment,
        issuer: &str,
    ) -> Result<Option<ClientRegistration>, AppError>;

    /// Keeps `registration` as the catalog app's client in this environment
    /// at its issuer, unless one is kept there already: then that one is
    /// returned, unchanged, and `registration` is dropped. One transaction,
    /// so concurrent first registrations keep exactly one. `NotFound` unless
    /// the app is in the organization's catalog.
    async fn save_organization_client_registration(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Environment,
        registration: &NewClientRegistration,
    ) -> Result<ClientRegistration, AppError>;

    /// Forgets only the refused client, never a newer registration or one
    /// in another organization, environment or workspace.
    async fn forget_organization_client_registration(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Environment,
        issuer: &str,
        client_id: &str,
    ) -> Result<(), AppError>;

    async fn forget_workspace_client_registration(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
        issuer: &str,
        client_id: &str,
    ) -> Result<(), AppError>;

    /// As [`AppStore::find_organization_client_registration`], for a workspace app.
    async fn find_workspace_client_registration(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
        issuer: &str,
    ) -> Result<Option<ClientRegistration>, AppError>;

    /// As [`AppStore::save_organization_client_registration`], for a workspace
    /// app: `NotFound` unless it is this workspace's own.
    async fn save_workspace_client_registration(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        app_id: AppId,
        registration: &NewClientRegistration,
    ) -> Result<ClientRegistration, AppError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_templates_as_the_contract_shapes_them() {
        let oauth = NewApp::blank("linear".into(), "linear".into(), String::new(), AuthType::OAuth2);
        assert_eq!(oauth.icon.glyph, "L");
        let json = serde_json::to_value(&oauth.auth).unwrap_or_default();
        assert_eq!(json["type"], "oauth2");
        assert_eq!(json["authorize_url"], serde_json::Value::Null);
        let token = NewApp::blank("échos".into(), "echos".into(), String::new(), AuthType::Token);
        assert_eq!(token.icon.glyph, "É");
        let json = serde_json::to_value(&token.auth).unwrap_or_default();
        assert_eq!(json["type"], "token");
        assert_eq!(json["placement"]["location"], "header");
        assert_eq!(json["inputs"][0]["key"], "token");
        let back: AuthTemplate = serde_json::from_value(json).unwrap_or(oauth.auth);
        assert_eq!(back, token.auth);
        let sign_in = NewApp::blank("Ledger".into(), "ledger".into(), String::new(), AuthType::McpOAuth);
        let json = serde_json::to_value(&sign_in.auth).unwrap_or_default();
        assert_eq!(
            json,
            serde_json::json!({ "type": "mcp_oauth", "scopes": [], "identity_url": null })
        );
        assert_eq!(sign_in.auth.type_name(), "mcp_oauth");
    }

    /// The mock's FNV-1a over the slug picks the same tile color.
    #[test]
    fn picks_the_mocks_color_for_a_slug() {
        // As the mock computes them: "linear" hashes to 0xd00594c0 (3 of 5),
        // "echo-server" to 0x5b050808 (2 of 5).
        for (slug, color) in [("linear", "#15803D"), ("echo-server", "#B45309")] {
            let app = NewApp::blank("A".into(), slug.into(), String::new(), AuthType::Token);
            assert_eq!(app.icon.color, color, "{slug}");
        }
    }
}
