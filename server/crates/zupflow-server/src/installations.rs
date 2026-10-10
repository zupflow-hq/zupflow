//! Installations (decision 0034): a workspace's connected accounts at
//! `/v1/workspaces/{ws}/installations`, and a catalog app's across the
//! organization at `/v1/organizations/{org}/apps/{app}/installations`.
//! They are made by Connect; here they are read, renamed, disconnected and
//! deleted, and their discovered tools are read and listed again. Their
//! credentials never leave the store through these routes.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::{Map, Value, json};
use zupflow_core::apps::{App, AppIcon, AuthTemplate, ToolEffect};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{
    AccountConnection, Installation, InstallationError, InstallationStatus, InstalledApp,
};
use zupflow_core::organizations::MemberRole;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{RegisteredTool, ToolError, ToolStatus};

use crate::apps::{app_failure, app_id};
use crate::auth::{Access, Actor, AppState, Caller, WorkspaceAccess};
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Fields};
use crate::workspaces::{ListQuery, invalid_cursor, listed_environment, page_limit};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/workspaces/{ws}/installations", get(list_installations))
        .route(
            "/v1/workspaces/{ws}/installations/{inst}",
            get(get_installation)
                .patch(rename_installation)
                .delete(delete_installation),
        )
        .route(
            "/v1/workspaces/{ws}/installations/{inst}/disconnect",
            post(disconnect_installation),
        )
        .route(
            "/v1/workspaces/{ws}/installations/{inst}/tools",
            get(list_installation_tools),
        )
        .route(
            "/v1/workspaces/{ws}/installations/{inst}/refresh-tools",
            post(refresh_installation_tools),
        )
        .route(
            "/v1/organizations/{org}/apps/{app}/installations",
            get(list_app_installations),
        )
}

const LABEL_MAX: usize = 40;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum OwnerScope {
    Organization,
    Workspace,
}

/// The contract's `InstallationApp`.
#[derive(Serialize)]
pub(crate) struct InstallationAppView {
    id: AppId,
    name: String,
    slug: String,
    owner_scope: OwnerScope,
    icon: AppIcon,
    auth_type: &'static str,
}

/// The contract's `Installation`.
#[derive(Serialize)]
pub(crate) struct InstallationView {
    id: InstallationId,
    workspace_id: WorkspaceId,
    app: InstallationAppView,
    label: String,
    status: InstallationStatus,
    configuration_version: u32,
    connection: Option<AccountConnection>,
    grant_count: u64,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// An installed app as the contract shows it beside an installation or a
/// Connect session.
pub(crate) fn installation_app_view(app: &App, installed: InstalledApp) -> InstallationAppView {
    InstallationAppView {
        id: app.id,
        name: app.name.clone(),
        slug: app.slug.clone(),
        owner_scope: match installed {
            InstalledApp::Catalog(_) => OwnerScope::Organization,
            InstalledApp::Workspace(_) => OwnerScope::Workspace,
        },
        icon: app.icon.clone(),
        auth_type: match app.auth {
            AuthTemplate::OAuth2 { .. } => "oauth2",
            AuthTemplate::Token { .. } => "token",
        },
    }
}

/// The apps an installation may be of: the organization's catalog and the
/// workspace's own, by ID.
struct Apps(HashMap<AppId, App>);

impl Apps {
    async fn of(
        state: &AppState,
        organization_id: OrganizationId,
        workspace_id: Option<WorkspaceId>,
    ) -> Result<Self, Failure> {
        let mut apps: HashMap<AppId, App> = state
            .apps
            .list_organization_apps(organization_id)
            .await
            .map_err(app_failure)?
            .into_iter()
            .map(|app| (app.id, app))
            .collect();
        if let Some(workspace_id) = workspace_id {
            for app in state
                .apps
                .list_workspace_apps(organization_id, workspace_id)
                .await
                .map_err(app_failure)?
            {
                apps.insert(app.id, app);
            }
        }
        Ok(Self(apps))
    }

    /// The installation as the contract shows it. Its app is always there:
    /// the database refuses to drop an app out from under an installation.
    fn view(&self, installation: Installation) -> Result<InstallationView, Failure> {
        let app = self.0.get(&installation.app.id()).ok_or_else(|| {
            tracing::error!(event = "installations.app_missing", installation_id = %installation.id);
            Failure(ApiError::internal())
        })?;
        Ok(installation_view(app, installation))
    }
}

/// An installation of `app` as the contract shows it.
pub(crate) fn installation_view(app: &App, installation: Installation) -> InstallationView {
    InstallationView {
        id: installation.id,
        workspace_id: installation.workspace_id,
        app: installation_app_view(app, installation.app),
        label: installation.label,
        status: installation.status,
        configuration_version: installation.configuration_version,
        connection: installation.connection,
        // Grants arrive with agents.
        grant_count: 0,
        created_at: installation.created_at,
        updated_at: installation.updated_at,
    }
}

fn installation_failure(error: InstallationError) -> Failure {
    match error {
        InstallationError::NotFound => ApiError::not_found("Installation").into(),
        InstallationError::InvalidCursor => invalid_cursor(),
        // The store has logged the cause.
        InstallationError::Unavailable => ApiError::new(
            503,
            "unavailable",
            "The service is unavailable right now. Try again shortly.",
        )
        .into(),
    }
}

/// An installation ID from the path; anything else is the same `404` as an
/// installation that isn't there.
fn installation_id(inst: &str) -> Result<InstallationId, Failure> {
    InstallationId::parse(inst).map_err(|_| ApiError::not_found("Installation").into())
}

async fn find(state: &AppState, access: &WorkspaceAccess, inst: &str) -> Result<Installation, Failure> {
    state
        .installations
        .find_installation(
            access.access.organization_id,
            access.workspace.id,
            installation_id(inst)?,
        )
        .await
        .map_err(installation_failure)?
        .ok_or_else(|| installation_failure(InstallationError::NotFound))
}

async fn list_installations(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let installations = state
        .installations
        .list_installations(organization_id, workspace_id)
        .await
        .map_err(installation_failure)?;
    let apps = Apps::of(&state, organization_id, Some(workspace_id)).await?;
    let views = installations
        .into_iter()
        .map(|installation| apps.view(installation))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(envelope(StatusCode::OK, views))
}

async fn get_installation(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, inst)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let installation = find(&state, &access, &inst).await?;
    let apps = Apps::of(&state, access.access.organization_id, Some(access.workspace.id)).await?;
    Ok(envelope(StatusCode::OK, apps.view(installation)?))
}

async fn rename_installation(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, inst)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(MemberRole::is_developer, "rename installations")?;
    let installation = find(&state, &access, &inst).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let label = body
        .string("label", &mut fields)
        .map(|label| label.trim().to_owned())
        .and_then(|label| {
            if label.is_empty() {
                fields.invalid("label", "Required.");
                None
            } else if label.chars().count() > LABEL_MAX {
                fields.invalid("label", "Use 40 characters or fewer.");
                None
            } else {
                Some(label)
            }
        });
    fields.finish()?;
    let Some(label) = label else {
        return Err(ApiError::internal().into());
    };
    let renamed = state
        .installations
        .rename_installation(
            access.access.organization_id,
            access.workspace.id,
            installation.id,
            &label,
        )
        .await
        .map_err(installation_failure)?;
    let apps = Apps::of(&state, access.access.organization_id, Some(access.workspace.id)).await?;
    Ok(envelope(StatusCode::OK, apps.view(renamed)?))
}

async fn disconnect_installation(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, inst)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(MemberRole::is_developer, "disconnect installations")?;
    let installation = find(&state, &access, &inst).await?;
    let disconnected = state
        .installations
        .disconnect_installation(access.access.organization_id, access.workspace.id, installation.id)
        .await
        .map_err(installation_failure)?;
    let apps = Apps::of(&state, access.access.organization_id, Some(access.workspace.id)).await?;
    Ok(envelope(StatusCode::OK, apps.view(disconnected)?))
}

async fn delete_installation(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, inst)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_admin, "remove installations")?;
    let installation = find(&state, &access, &inst).await?;
    state
        .installations
        .delete_installation(access.access.organization_id, access.workspace.id, installation.id)
        .await
        .map_err(installation_failure)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The contract's `InstallationToolStatus`.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ToolStatusView {
    Current,
    Changed,
    Gone,
}

/// The contract's `ToolText`: everything a definition says, as hashed.
#[derive(Serialize)]
struct ToolText {
    title: Option<String>,
    description: Option<String>,
    input_schema: Option<Map<String, Value>>,
    output_schema: Option<Map<String, Value>>,
    annotations: Option<Map<String, Value>>,
}

/// The contract's `InstallationTool`.
#[derive(Serialize)]
struct InstallationToolView {
    name: String,
    effect: ToolEffect,
    status: ToolStatusView,
    pinned: ToolText,
    latest: Option<ToolText>,
    first_seen_at: Timestamp,
    seen_at: Timestamp,
    changed_at: Option<Timestamp>,
    gone_at: Option<Timestamp>,
}

/// What a stored definition says. The registry holds the canonical JSON the
/// MCP client made; a field missing, or of another type, reads as `null`.
fn tool_text(definition: &str) -> ToolText {
    let parsed: Map<String, Value> = serde_json::from_str(definition).unwrap_or_else(|_| {
        tracing::error!(event = "installations.tool_definition_unreadable");
        Map::new()
    });
    let text = |key: &str| parsed.get(key).and_then(Value::as_str).map(str::to_owned);
    let object = |key: &str| parsed.get(key).and_then(Value::as_object).cloned();
    ToolText {
        title: text("title"),
        description: text("description"),
        input_schema: object("inputSchema"),
        output_schema: object("outputSchema"),
        annotations: object("annotations"),
    }
}

fn tool_view(tool: RegisteredTool) -> InstallationToolView {
    InstallationToolView {
        status: match tool.status() {
            ToolStatus::Current => ToolStatusView::Current,
            ToolStatus::Changed => ToolStatusView::Changed,
            ToolStatus::Gone => ToolStatusView::Gone,
        },
        pinned: tool_text(&tool.pinned.definition),
        latest: tool.latest.as_ref().map(|latest| tool_text(&latest.definition)),
        name: tool.name,
        effect: tool.effect,
        first_seen_at: tool.first_seen_at,
        seen_at: tool.seen_at,
        changed_at: tool.changed_at,
        gone_at: tool.gone_at,
    }
}

fn tool_failure(error: ToolError) -> Failure {
    match error {
        ToolError::NotFound => ApiError::not_found("Installation").into(),
        // Only recording a listing refuses one; reading never does.
        ToolError::InvalidListing => ApiError::internal().into(),
        // The store has logged the cause.
        ToolError::Unavailable => ApiError::new(
            503,
            "unavailable",
            "The service is unavailable right now. Try again shortly.",
        )
        .into(),
    }
}

/// A tool list's cursor: the last name of the page before, so the next page
/// starts after it. Opaque to callers.
fn tool_cursor(name: &str) -> String {
    URL_SAFE_NO_PAD.encode(name)
}

fn tool_after(cursor: &str) -> Result<String, Failure> {
    URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|name| !name.is_empty())
        .ok_or_else(invalid_cursor)
}

/// A page of the tools the installation's app listed to it, by name. Any member reads them.
async fn list_installation_tools(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, inst)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let installation = find(&state, &access, &inst).await?;
    let query = ListQuery::parse(raw.as_deref())?;
    let after = query
        .cursor
        .as_deref()
        .filter(|cursor| !cursor.is_empty())
        .map(tool_after)
        .transpose()?;
    let page = state
        .tools
        .list_tools(
            access.access.organization_id,
            access.workspace.id,
            installation.id,
            after.as_deref(),
            page_limit(&query),
        )
        .await
        .map_err(tool_failure)?;
    let next_cursor = page.next_after.as_deref().map(tool_cursor);
    let views: Vec<InstallationToolView> = page.items.into_iter().map(tool_view).collect();
    let meta = json!({ "pagination": { "next_cursor": next_cursor, "total": page.total } });
    Ok(envelope_with_meta(StatusCode::OK, views, meta))
}

/// How long after an accepted refresh the next one is refused.
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

fn unavailable_refresh() -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "Tools couldn't be refreshed right now. Try again shortly.",
    )
    .into()
}

/// Asks for a connected installation's tools to be listed again. The listing
/// is background work: this answers once the job is queued.
async fn refresh_installation_tools(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, inst)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_developer, "refresh tools")?;
    // In Production the Console only observes: the customer's product
    // refreshes there, with an API key.
    if matches!(access.access.actor, Actor::Member(_)) && access.workspace.environment == Environment::Production {
        return Err(ApiError::new(
            403,
            "console_read_only_in_production",
            "The Console can't refresh tools in Production. Your product does that through the API.",
        )
        .into());
    }
    let installation = find(&state, &access, &inst).await?;
    if installation.status != InstallationStatus::Connected || installation.connection.is_none() {
        return Err(ApiError::new(
            409,
            "installation_not_connected",
            "This installation isn't connected, so its tools can't be listed. Reconnect it first.",
        )
        .into());
    }
    // One accepted refresh per installation a minute, claimed atomically.
    let scope = Scope::Workspace(access.access.organization_id, access.workspace.id);
    let key = Name::new(&format!("refresh-tools:{}", installation.id)).map_err(|_| Failure(ApiError::internal()))?;
    let claimed = state
        .cache
        .add_if_absent(&scope, &key, Vec::new(), REFRESH_INTERVAL)
        .await
        .map_err(|error| {
            tracing::warn!(event = "installations.refresh_pacing_unavailable", installation_id = %installation.id, %error);
            unavailable_refresh()
        })?;
    if !claimed {
        return Err(ApiError::new(
            429,
            "refresh_too_soon",
            "These tools were refreshed less than a minute ago. Try again in a minute, as a new request.",
        )
        .into());
    }
    // Nothing queued is no refresh: answer 503, which idempotency never saves,
    // and release the claim so a retry isn't refused as too soon.
    if crate::discovery::enqueue(
        &state,
        access.access.organization_id,
        access.workspace.id,
        installation.id,
    )
    .await
    .is_err()
    {
        if let Err(error) = state.cache.delete(&scope, &key).await {
            tracing::warn!(event = "installations.refresh_release_failed", installation_id = %installation.id, %error);
        }
        return Err(unavailable_refresh());
    }
    let apps = Apps::of(&state, access.access.organization_id, Some(access.workspace.id)).await?;
    Ok(envelope(StatusCode::OK, apps.view(installation)?))
}

/// The contract's `WorkspaceRef`.
#[derive(Serialize)]
struct WorkspaceRef {
    id: WorkspaceId,
    name: String,
    slug: String,
    environment: Environment,
}

/// The contract's `AppInstallationRef`.
#[derive(Serialize)]
struct AppInstallationView {
    installation: InstallationView,
    workspace: WorkspaceRef,
}

async fn list_app_installations(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, app)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let app = state
        .apps
        .find_organization_app(access.organization_id, app_id(&app)?)
        .await
        .map_err(app_failure)?
        .ok_or_else(|| Failure(ApiError::not_found("App")))?;
    let query = ListQuery::parse(raw.as_deref())?;
    let environment = listed_environment(&access, &query)?;
    let page = state
        .installations
        .list_app_installations(
            access.organization_id,
            app.id,
            environment,
            query.cursor.as_deref().filter(|cursor| !cursor.is_empty()),
            page_limit(&query),
        )
        .await
        .map_err(installation_failure)?;
    // A catalog app's installations are all of this one app.
    let apps = Apps(HashMap::from([(app.id, app)]));
    let items = page
        .items
        .into_iter()
        .map(|item| {
            let workspace = WorkspaceRef {
                id: item.installation.workspace_id,
                name: item.workspace_name,
                slug: item.workspace_slug,
                environment: item.environment,
            };
            Ok(AppInstallationView {
                installation: apps.view(item.installation)?,
                workspace,
            })
        })
        .collect::<Result<Vec<_>, Failure>>()?;
    let meta = json!({ "pagination": { "next_cursor": page.next_cursor, "total": page.total } });
    Ok(envelope_with_meta(StatusCode::OK, items, meta))
}
