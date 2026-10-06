//! Apps (decision 0031): the organization's catalog at
//! `/v1/organizations/{org}/apps`, and a workspace's own apps at
//! `/v1/workspaces/{ws}/apps`. Neither route reaches the other's apps: a
//! workspace app is never found in the catalog, nor a catalog app, or another
//! workspace's app, under a workspace.
//!
//! An app's client configurations are in `app_configurations`; nothing is
//! installed yet, so every app's `installation_count` is 0.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use serde::Serialize;
use zupflow_core::apps::{
    App, AppChanges, AppError, AppIcon, AppKind, AuthTemplate, AuthType, Configuration, ConfigurationStatus, NewApp,
    ToolDefinition,
};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AppId, OrganizationId, WorkspaceId};
use zupflow_core::organizations::MemberRole;
use zupflow_core::time::Timestamp;

use crate::auth::{Access, AppState, Caller, WorkspaceAccess};
use crate::http::{Failure, envelope};
use crate::input::{Body, Fields, Object};
use crate::organizations::{checked_name, checked_slug};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/organizations/{org}/apps",
            get(list_organization_apps).post(create_organization_app),
        )
        .route(
            "/v1/organizations/{org}/apps/{app}",
            get(get_organization_app).patch(update_organization_app),
        )
        .route(
            "/v1/workspaces/{ws}/apps",
            get(list_workspace_apps).post(create_workspace_app),
        )
        .route(
            "/v1/workspaces/{ws}/apps/{app}",
            get(get_workspace_app).patch(update_workspace_app),
        )
}

const DESCRIPTION_MAX: usize = 280;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum OwnerScope {
    Organization,
    Workspace,
}

/// The contract's `ConfigurationSummary`.
#[derive(Serialize)]
pub(crate) struct ConfigurationSummary {
    environment: Environment,
    status: ConfigurationStatus,
    version: u32,
    updated_at: Option<Timestamp>,
}

impl ConfigurationSummary {
    pub(crate) fn of(configuration: &Configuration, auth: &AuthTemplate) -> Self {
        Self {
            environment: configuration.environment,
            status: configuration.status(auth),
            version: configuration.version,
            updated_at: configuration.updated_at,
        }
    }
}

/// The contract's `App`.
#[derive(Serialize)]
pub(crate) struct AppView {
    id: AppId,
    organization_id: OrganizationId,
    owner_workspace_id: Option<WorkspaceId>,
    owner_scope: OwnerScope,
    kind: AppKind,
    name: String,
    slug: String,
    description: String,
    icon: AppIcon,
    auth: AuthTemplate,
    tools: Vec<ToolDefinition>,
    version: u32,
    configurations: Vec<ConfigurationSummary>,
    installation_count: u64,
    created_at: Timestamp,
    updated_at: Timestamp,
}

/// The app as the contract shows it, in `environments`: both for a catalog
/// app, the workspace's own for a workspace app. `configurations` may hold
/// any app's; this app's are picked out, and an environment never written
/// reads as empty.
pub(crate) fn view(app: App, configurations: &[Configuration], environments: &[Environment]) -> AppView {
    let summaries = environments
        .iter()
        .map(|&environment| {
            let empty = Configuration::empty(app.id, environment);
            let configuration = configurations
                .iter()
                .find(|configuration| configuration.app_id == app.id && configuration.environment == environment)
                .unwrap_or(&empty);
            ConfigurationSummary::of(configuration, &app.auth)
        })
        .collect();
    AppView {
        id: app.id,
        organization_id: app.organization_id,
        owner_scope: if app.owner_workspace_id.is_some() {
            OwnerScope::Workspace
        } else {
            OwnerScope::Organization
        },
        owner_workspace_id: app.owner_workspace_id,
        kind: app.kind,
        name: app.name,
        slug: app.slug,
        description: app.description,
        icon: app.icon,
        auth: app.auth,
        tools: app.tools,
        version: app.version,
        configurations: summaries,
        installation_count: 0,
        created_at: app.created_at,
        updated_at: app.updated_at,
    }
}

/// The environments a catalog app is shown in: both for a member, only its
/// own for an API key, which sees nothing of the other (decision 0029).
pub(crate) fn catalog_environments(access: &Access) -> Vec<Environment> {
    match access.environment() {
        Some(own) => vec![own],
        None => vec![Environment::Sandbox, Environment::Production],
    }
}

pub(crate) fn app_failure(error: AppError) -> Failure {
    match error {
        AppError::NotFound => ApiError::not_found("App").into(),
        // Only a configuration write meets this, and only after retrying.
        AppError::SecretVersionTaken => ApiError::new(
            409,
            "configuration_conflict",
            "The client secret changed while this request ran. Try again.",
        )
        .into(),
        AppError::SlugTaken => {
            let mut error = ApiError::new(409, "slug_taken", "That slug is already used by another app.");
            error.fields = Some([("slug".to_owned(), "Already used by another app here.".to_owned())].into());
            error.into()
        }
        // The store has logged the cause.
        AppError::Unavailable => ApiError::new(
            503,
            "unavailable",
            "The service is unavailable right now. Try again shortly.",
        )
        .into(),
    }
}

/// An app ID from the path; anything else is the same `404` as an app that
/// isn't there.
pub(crate) fn app_id(app: &str) -> Result<AppId, Failure> {
    AppId::parse(app).map_err(|_| ApiError::not_found("App").into())
}

/// A description: trimmed, at most 280 characters, possibly empty.
fn checked_description(value: Option<String>, fields: &mut Fields) -> Option<String> {
    let description = value?.trim().to_owned();
    if description.chars().count() > DESCRIPTION_MAX {
        fields.invalid("description", "Use 280 characters or fewer.");
        None
    } else {
        Some(description)
    }
}

/// `CreateAppInput`, checked field by field; then the kind, which is a string
/// to the body's rules and `unsupported_app_kind` unless it is `mcp`.
fn new_app(body: &Object) -> Result<NewApp, Failure> {
    let mut fields = Fields::default();
    let kind = body.string("kind", &mut fields);
    let name = checked_name(body.string("name", &mut fields), &mut fields);
    let slug = checked_slug(body.string("slug", &mut fields), &mut fields);
    let description = checked_description(body.string("description", &mut fields), &mut fields);
    let auth_type = body.optional_string("auth_type", &mut fields).and_then(|value| {
        let parsed = AuthType::parse(&value);
        if parsed.is_none() {
            fields.invalid("auth_type", "Choose OAuth or token.");
        }
        parsed
    });
    if !body.has("auth_type") {
        fields.invalid("auth_type", "Choose OAuth or token.");
    }
    fields.finish()?;
    let (Some(kind), Some(name), Some(slug), Some(description), Some(auth_type)) =
        (kind, name, slug, description, auth_type)
    else {
        return Err(ApiError::internal().into());
    };
    if AppKind::parse(&kind).is_none() {
        let mut error = ApiError::new(
            400,
            "unsupported_app_kind",
            format!("App kind \"{kind}\" is not supported."),
        );
        error.fields = Some([("kind".to_owned(), "Only \"mcp\" is supported.".to_owned())].into());
        return Err(error.into());
    }
    Ok(NewApp::blank(name, slug, description, auth_type))
}

/// `UpdateAppInput`: a name and a description, each optional.
fn changes(body: &Object) -> Result<(Option<String>, Option<String>), Failure> {
    let mut fields = Fields::default();
    let name = checked_name(body.optional_string("name", &mut fields), &mut fields);
    let description = checked_description(body.optional_string("description", &mut fields), &mut fields);
    fields.finish()?;
    Ok((name, description))
}

async fn list_organization_apps(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let apps = state
        .apps
        .list_organization_apps(access.organization_id)
        .await
        .map_err(app_failure)?;
    let configurations = state
        .apps
        .list_organization_configurations(access.organization_id)
        .await
        .map_err(app_failure)?;
    let views: Vec<AppView> = apps
        .into_iter()
        .map(|app| view(app, &configurations, &catalog_environments(&access)))
        .collect();
    Ok(envelope(StatusCode::OK, views))
}

async fn create_organization_app(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "add apps to the catalog")?;
    let app = new_app(&body.object()?)?;
    let app = state
        .apps
        .create_organization_app(access.organization_id, AppId::generate(), &app)
        .await
        .map_err(app_failure)?;
    Ok(envelope(
        StatusCode::CREATED,
        view(app, &[], &catalog_environments(&access)),
    ))
}

async fn get_organization_app(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, app)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let app = state
        .apps
        .find_organization_app(access.organization_id, app_id(&app)?)
        .await
        .map_err(app_failure)?
        .ok_or(AppError::NotFound)
        .map_err(app_failure)?;
    let configurations = state
        .apps
        .list_organization_configurations(access.organization_id)
        .await
        .map_err(app_failure)?;
    Ok(envelope(
        StatusCode::OK,
        view(app, &configurations, &catalog_environments(&access)),
    ))
}

async fn update_organization_app(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, app)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "edit apps")?;
    let app_id = app_id(&app)?;
    // A missing app is 404 before anything about the body.
    state
        .apps
        .find_organization_app(access.organization_id, app_id)
        .await
        .map_err(app_failure)?
        .ok_or(AppError::NotFound)
        .map_err(app_failure)?;
    let (name, description) = changes(&body.object()?)?;
    let app = state
        .apps
        .update_organization_app(
            access.organization_id,
            app_id,
            AppChanges {
                name: name.as_deref(),
                description: description.as_deref(),
            },
        )
        .await
        .map_err(app_failure)?;
    let configurations = state
        .apps
        .list_organization_configurations(access.organization_id)
        .await
        .map_err(app_failure)?;
    Ok(envelope(
        StatusCode::OK,
        view(app, &configurations, &catalog_environments(&access)),
    ))
}

async fn list_workspace_apps(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let apps = state
        .apps
        .list_workspace_apps(access.access.organization_id, access.workspace.id)
        .await
        .map_err(app_failure)?;
    let configurations = state
        .apps
        .list_workspace_configurations(access.access.organization_id, access.workspace.id)
        .await
        .map_err(app_failure)?;
    let environment = [access.workspace.environment];
    let views: Vec<AppView> = apps
        .into_iter()
        .map(|app| view(app, &configurations, &environment))
        .collect();
    Ok(envelope(StatusCode::OK, views))
}

async fn create_workspace_app(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_admin, "create workspace apps")?;
    let app = new_app(&body.object()?)?;
    let app = state
        .apps
        .create_workspace_app(
            access.access.organization_id,
            access.workspace.id,
            AppId::generate(),
            &app,
        )
        .await
        .map_err(app_failure)?;
    Ok(envelope(
        StatusCode::CREATED,
        view(app, &[], &[access.workspace.environment]),
    ))
}

async fn get_workspace_app(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, app)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let app = state
        .apps
        .find_workspace_app(access.access.organization_id, access.workspace.id, app_id(&app)?)
        .await
        .map_err(app_failure)?
        .ok_or(AppError::NotFound)
        .map_err(app_failure)?;
    let configurations = state
        .apps
        .list_workspace_configurations(access.access.organization_id, access.workspace.id)
        .await
        .map_err(app_failure)?;
    Ok(envelope(
        StatusCode::OK,
        view(app, &configurations, &[access.workspace.environment]),
    ))
}

async fn update_workspace_app(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, app)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_admin, "edit apps")?;
    let (organization_id, workspace_id) = (access.access.organization_id, access.workspace.id);
    let app_id = app_id(&app)?;
    state
        .apps
        .find_workspace_app(organization_id, workspace_id, app_id)
        .await
        .map_err(app_failure)?
        .ok_or(AppError::NotFound)
        .map_err(app_failure)?;
    let (name, description) = changes(&body.object()?)?;
    let app = state
        .apps
        .update_workspace_app(
            organization_id,
            workspace_id,
            app_id,
            AppChanges {
                name: name.as_deref(),
                description: description.as_deref(),
            },
        )
        .await
        .map_err(app_failure)?;
    let configurations = state
        .apps
        .list_workspace_configurations(access.access.organization_id, access.workspace.id)
        .await
        .map_err(app_failure)?;
    Ok(envelope(
        StatusCode::OK,
        view(app, &configurations, &[access.workspace.environment]),
    ))
}
