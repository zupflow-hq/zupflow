//! Installations (decision 0034): a workspace's connected accounts at
//! `/v1/workspaces/{ws}/installations`, and a catalog app's across the
//! organization at `/v1/organizations/{org}/apps/{app}/installations`.
//! They are made by Connect; here they are read, renamed, disconnected and
//! deleted. Their credentials never leave the store through these routes.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::json;
use zupflow_core::apps::{App, AppIcon, AuthTemplate};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{
    AccountConnection, Installation, InstallationError, InstallationStatus, InstalledApp,
};
use zupflow_core::organizations::MemberRole;
use zupflow_core::time::Timestamp;

use crate::apps::{app_failure, app_id};
use crate::auth::{Access, AppState, Caller, WorkspaceAccess};
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
struct InstallationView {
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
        Ok(InstallationView {
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
        })
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
