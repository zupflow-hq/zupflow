//! Apps: MCP servers an organization offers in its catalog, or one workspace
//! keeps for itself. The two live apart (decision 0031): a catalog app belongs
//! to the organization, a workspace app to one workspace, and every
//! [`AppStore`] method names the organization first, then the workspace for a
//! workspace app.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::ids::{AppId, OrganizationId, WorkspaceId};
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
    Token {
        inputs: Vec<TokenInput>,
        placement: TokenPlacement,
        verify_url: Option<String>,
        instructions: String,
        docs_url: Option<String>,
    },
}

/// Which kind of [`AuthTemplate`] a new app starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    OAuth2,
    Token,
}

impl AuthType {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "oauth2" => Some(Self::OAuth2),
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AppError {
    #[error("the store is unavailable")]
    Unavailable,
    /// Another app of the same owner (the catalog, or one workspace) uses the slug.
    #[error("that slug is taken")]
    SlugTaken,
    #[error("not found")]
    NotFound,
}

#[async_trait]
pub trait AppStore: Send + Sync {
    /// The organization's catalog, by name.
    async fn list_organization_apps(&self, organization_id: OrganizationId) -> Result<Vec<App>, AppError>;

    async fn create_organization_app(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        app: &NewApp,
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
    ) -> Result<App, AppError>;
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
