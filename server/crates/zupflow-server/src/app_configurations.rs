//! Client configurations (decisions 0031 and 0033): a catalog app's at
//! `/v1/organizations/{org}/apps/{app}/environments/{env}/configuration`, a
//! workspace app's, in its workspace's environment, at
//! `/v1/workspaces/{ws}/apps/{app}/configuration`; and the apps a workspace
//! can install, at `/v1/workspaces/{ws}/available-apps`.
//!
//! The OAuth client secret is sealed under a context naming its owner,
//! environment, app and version before it reaches the store, and no response
//! ever carries it: only whether it is set, its last four characters and when
//! it changed.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use serde::Serialize;
use serde_json::Value;
use zupflow_core::apps::{
    App, AppError, AuthTemplate, Configuration, ConfigurationChanges, ConfigurationStatus, NewClientSecret,
};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AppId, UserId, WorkspaceId};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::organizations::{MemberRole, User};
use zupflow_core::outbound::is_public;
use zupflow_core::time::Timestamp;

use crate::apps::{
    AppView, ConfigurationSummary, app_failure, app_id, catalog_environments, installation_counts, view,
};
use crate::auth::{Access, Actor, AppState, Caller, WorkspaceAccess};
use crate::http::{Failure, envelope};
use crate::input::{Body, Fields, Object};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/organizations/{org}/apps/{app}/environments/{env}/configuration",
            get(get_organization_configuration).put(update_organization_configuration),
        )
        .route(
            "/v1/workspaces/{ws}/apps/{app}/configuration",
            get(get_workspace_configuration).put(update_workspace_configuration),
        )
        .route("/v1/workspaces/{ws}/available-apps", get(list_available_apps))
}

/// Where providers send the browser back after consent: the same for every
/// app and environment, so an organization registers it once per client.
pub fn redirect_uri(public_url: &str) -> String {
    format!("{public_url}/connect/oauth/callback")
}

#[derive(Serialize)]
struct McpView {
    server_url: Option<String>,
    transport: &'static str,
}

#[derive(Serialize)]
struct SecretView {
    set: bool,
    last4: Option<String>,
    updated_at: Option<Timestamp>,
}

#[derive(Serialize)]
struct OAuthView {
    client_id: Option<String>,
    client_secret: SecretView,
    redirect_uri: String,
    scopes: Vec<String>,
}

/// The contract's `ClientConfiguration`.
#[derive(Serialize)]
struct ConfigurationView {
    app_id: AppId,
    environment: Environment,
    status: ConfigurationStatus,
    version: u32,
    mcp: McpView,
    /// Only for an OAuth app.
    oauth: Option<OAuthView>,
    missing: Vec<&'static str>,
    updated_at: Option<Timestamp>,
    updated_by: Option<User>,
}

fn configuration_view(configuration: Configuration, auth: &AuthTemplate, public_url: &str) -> ConfigurationView {
    let status = configuration.status(auth);
    let missing = configuration.missing(auth);
    let oauth = matches!(auth, AuthTemplate::OAuth2 { .. }).then(|| OAuthView {
        client_id: configuration.oauth_client_id.clone(),
        client_secret: SecretView {
            set: configuration.client_secret.is_some(),
            last4: configuration.client_secret.as_ref().map(|secret| secret.last4.clone()),
            updated_at: configuration.client_secret.as_ref().map(|secret| secret.updated_at),
        },
        redirect_uri: redirect_uri(public_url),
        scopes: configuration.oauth_scopes.clone(),
    });
    ConfigurationView {
        app_id: configuration.app_id,
        environment: configuration.environment,
        status,
        version: configuration.version,
        mcp: McpView {
            server_url: configuration.mcp_server_url,
            transport: "streamable_http",
        },
        oauth,
        missing,
        updated_at: configuration.updated_at,
        updated_by: configuration.updated_by,
    }
}

/// `UpdateConfigurationInput`, checked field by field.
struct Input {
    mcp_server_url: Option<Option<String>>,
    oauth_client_id: Option<Option<String>>,
    oauth_client_secret: Option<String>,
    oauth_scopes: Option<Vec<String>>,
}

/// A nullable string: trimmed, at most `max` characters; blank or `null` clears it.
fn clearable(body: &Object, field: &str, max: usize, fields: &mut Fields) -> Option<Option<String>> {
    match body.0.get(field)? {
        Value::Null => Some(None),
        Value::String(value) => {
            let value = value.trim();
            if value.chars().count() > max {
                fields.invalid(field, &format!("Use {max} characters or fewer."));
                None
            } else {
                Some((!value.is_empty()).then(|| value.to_owned()))
            }
        }
        _ => {
            fields.invalid(field, "Must be a string or null.");
            None
        }
    }
}

fn input(body: &Object) -> Result<Input, Failure> {
    let mut fields = Fields::default();
    let mcp_server_url = clearable(body, "mcp_server_url", 500, &mut fields);
    let oauth_client_id = clearable(body, "oauth_client_id", 300, &mut fields);
    let oauth_client_secret = body
        .optional_string("oauth_client_secret", &mut fields)
        .and_then(|secret| match secret.chars().count() {
            0..8 => {
                fields.invalid("oauth_client_secret", "Paste the full client secret.");
                None
            }
            501.. => {
                fields.invalid("oauth_client_secret", "Use 500 characters or fewer.");
                None
            }
            _ => Some(secret),
        });
    let oauth_scopes = match body.0.get("oauth_scopes") {
        None => None,
        Some(Value::Array(items)) if items.len() > 50 => {
            fields.invalid("oauth_scopes", "Use 50 scopes or fewer.");
            None
        }
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(scope) if scope.chars().count() <= 300 => Some(scope.clone()),
                _ => None,
            })
            .collect::<Option<Vec<String>>>()
            .or_else(|| {
                fields.invalid("oauth_scopes", "Each scope is a string of 300 characters or fewer.");
                None
            }),
        Some(_) => {
            fields.invalid("oauth_scopes", "Must be a list of scopes.");
            None
        }
    };
    fields.finish()?;
    Ok(Input {
        mcp_server_url,
        oauth_client_id,
        oauth_client_secret,
        oauth_scopes,
    })
}

fn field_error(status: u16, code: &'static str, message: &str, field: &str, detail: &str) -> Failure {
    let mut error = ApiError::new(status, code, message);
    error.fields = Some([(field.to_owned(), detail.to_owned())].into());
    error.into()
}

/// A provider destination: an absolute `https` URL to a public host. Here,
/// at save time, IP literals that aren't public, a few names that always mean
/// this network (`localhost`, `.local`, `.internal`, metadata hosts) and
/// credentials in the URL are refused. That is a courtesy, not the guard: a
/// name can resolve anywhere, so the outbound path checks every resolved
/// address on every call (decision 0032).
pub(crate) fn check_public_https(value: &str, field: &str) -> Result<(), Failure> {
    let Ok(url) = url::Url::parse(value) else {
        return Err(field_error(
            400,
            "validation_failed",
            "Some fields are invalid.",
            field,
            "Enter a full URL, including https://.",
        ));
    };
    if url.scheme() != "https" {
        return Err(field_error(
            422,
            "insecure_url",
            "Provider URLs must use HTTPS.",
            field,
            "Use an https:// URL.",
        ));
    }
    let private = match url.host() {
        None => true,
        Some(url::Host::Ipv4(ip)) => !is_public(ip.into()),
        Some(url::Host::Ipv6(ip)) => !is_public(ip.into()),
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost"
                || [".localhost", ".local", ".internal"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
                || name.starts_with("metadata.")
        }
    };
    if private || !url.username().is_empty() || url.password().is_some() {
        return Err(field_error(
            422,
            "destination_not_allowed",
            "Private and link-local hosts are blocked.",
            field,
            "Use a public host. Private, loopback and link-local addresses are blocked.",
        ));
    }
    Ok(())
}

/// The checks that need the app: the endpoint, OAuth on a token app, and
/// scopes the app doesn't offer.
fn check_against(app: &App, input: &Input) -> Result<(), Failure> {
    if let Some(Some(url)) = &input.mcp_server_url {
        check_public_https(url, "mcp_server_url")?;
    }
    match &app.auth {
        AuthTemplate::Token { .. } => {
            if matches!(input.oauth_client_id, Some(Some(_))) || input.oauth_client_secret.is_some() {
                return Err(ApiError::new(
                    422,
                    "oauth_not_supported",
                    format!("{} authenticates with a token, not OAuth.", app.name),
                )
                .into());
            }
        }
        AuthTemplate::OAuth2 { available_scopes, .. } => {
            let unknown = input
                .oauth_scopes
                .iter()
                .flatten()
                .find(|scope| !available_scopes.iter().any(|offered| &offered.name == *scope));
            if let Some(unknown) = unknown {
                let message = format!("{unknown} is not offered by this app.");
                return Err(field_error(422, "unknown_scope", &message, "oauth_scopes", &message));
            }
        }
    }
    Ok(())
}

/// The member writing; an API key writes as nobody.
fn writer(access: &Access) -> Option<UserId> {
    match &access.actor {
        Actor::Member(member) => Some(member.user.id),
        Actor::ApiKey(_) => None,
    }
}

fn unavailable() -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

/// Whose configuration a write is, for the store and the secret's context.
#[derive(Clone, Copy)]
enum Owner {
    Catalog(Environment),
    Workspace(WorkspaceId, Environment),
}

/// Seals the new secret, if any, under the version after the stored one and
/// saves. Another write may take that version first; then this one reads the
/// new version and tries again.
async fn save(
    state: &AppState,
    access: &Access,
    owner: Owner,
    app: &App,
    input: Input,
) -> Result<Configuration, Failure> {
    let organization_id = access.organization_id;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let current = match owner {
            Owner::Catalog(environment) => {
                state
                    .apps
                    .find_organization_configuration(organization_id, app.id, environment)
                    .await
            }
            Owner::Workspace(workspace_id, _) => {
                state
                    .apps
                    .find_workspace_configuration(organization_id, workspace_id, app.id)
                    .await
            }
        }
        .map_err(app_failure)?;
        let client_secret = match &input.oauth_client_secret {
            None => None,
            Some(secret) => {
                let version = current.as_ref().map_or(0, |configuration| configuration.secret_version) + 1;
                let (workspace_id, environment) = match owner {
                    Owner::Catalog(environment) => (None, environment),
                    Owner::Workspace(workspace_id, environment) => (Some(workspace_id), environment),
                };
                let context =
                    EncryptionContext::oauth_client_secret(organization_id, workspace_id, environment, app.id, version);
                let sealed = keys::seal(state.keys.as_ref(), &context, secret.as_bytes())
                    .await
                    .map_err(|error| {
                        tracing::error!(event = "app_configurations.seal_failed", %organization_id, %error);
                        unavailable()
                    })?;
                let last4: String = {
                    let characters: Vec<char> = secret.chars().collect();
                    characters[characters.len().saturating_sub(4)..].iter().collect()
                };
                Some(NewClientSecret { sealed, last4, version })
            }
        };
        let changes = ConfigurationChanges {
            mcp_server_url: input.mcp_server_url.clone(),
            oauth_client_id: input.oauth_client_id.clone(),
            client_secret,
            oauth_scopes: input.oauth_scopes.clone(),
            updated_by: writer(access),
        };
        let saved = match owner {
            Owner::Catalog(environment) => {
                state
                    .apps
                    .save_organization_configuration(organization_id, app.id, environment, &changes)
                    .await
            }
            Owner::Workspace(workspace_id, _) => {
                state
                    .apps
                    .save_workspace_configuration(organization_id, workspace_id, app.id, &changes)
                    .await
            }
        };
        match saved {
            Err(AppError::SecretVersionTaken) if attempts < 3 => continue,
            saved => return saved.map_err(app_failure),
        }
    }
}

/// The environment in the path; anything else is the same `404` as an
/// environment that doesn't exist. A key acts in its own only.
fn environment(access: &Access, env: &str) -> Result<Environment, Failure> {
    let environment = Environment::parse(env).ok_or_else(|| Failure(ApiError::not_found("Environment")))?;
    if access.environment().is_some_and(|own| own != environment) {
        return Err(ApiError::new(
            422,
            "environment_mismatch",
            "This API key works in one environment only; use a key for the other one.",
        )
        .into());
    }
    Ok(environment)
}

async fn catalog_app(state: &AppState, access: &Access, app: &str) -> Result<App, Failure> {
    state
        .apps
        .find_organization_app(access.organization_id, app_id(app)?)
        .await
        .map_err(app_failure)?
        .ok_or_else(|| app_failure(AppError::NotFound))
}

async fn workspace_app(state: &AppState, access: &WorkspaceAccess, app: &str) -> Result<App, Failure> {
    state
        .apps
        .find_workspace_app(access.access.organization_id, access.workspace.id, app_id(app)?)
        .await
        .map_err(app_failure)?
        .ok_or_else(|| app_failure(AppError::NotFound))
}

async fn get_organization_configuration(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, app, env)): Path<(String, String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let app = catalog_app(&state, &access, &app).await?;
    let environment = environment(&access, &env)?;
    let configuration = state
        .apps
        .find_organization_configuration(access.organization_id, app.id, environment)
        .await
        .map_err(app_failure)?
        .unwrap_or_else(|| Configuration::empty(app.id, environment));
    Ok(envelope(
        StatusCode::OK,
        configuration_view(configuration, &app.auth, &state.public_url),
    ))
}

async fn update_organization_configuration(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, app, env)): Path<(String, String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "configure apps")?;
    let app = catalog_app(&state, &access, &app).await?;
    let environment = environment(&access, &env)?;
    let input = input(&body.object()?)?;
    check_against(&app, &input)?;
    let saved = save(&state, &access, Owner::Catalog(environment), &app, input).await?;
    Ok(envelope(
        StatusCode::OK,
        configuration_view(saved, &app.auth, &state.public_url),
    ))
}

async fn get_workspace_configuration(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, app)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let app = workspace_app(&state, &access, &app).await?;
    let configuration = state
        .apps
        .find_workspace_configuration(access.access.organization_id, access.workspace.id, app.id)
        .await
        .map_err(app_failure)?
        .unwrap_or_else(|| Configuration::empty(app.id, access.workspace.environment));
    Ok(envelope(
        StatusCode::OK,
        configuration_view(configuration, &app.auth, &state.public_url),
    ))
}

async fn update_workspace_configuration(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, app)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access.access.require(MemberRole::is_admin, "configure apps")?;
    let app = workspace_app(&state, &access, &app).await?;
    let body = body.object()?;
    let own = access.workspace.environment;
    if body
        .0
        .get("environment")
        .is_some_and(|named| named.as_str() != Some(own.as_str()))
    {
        return Err(ApiError::new(
            422,
            "environment_mismatch",
            format!("Workspace apps inherit the workspace's environment ({}).", own.as_str()),
        )
        .into());
    }
    let input = input(&body)?;
    check_against(&app, &input)?;
    let saved = save(
        &state,
        &access.access,
        Owner::Workspace(access.workspace.id, own),
        &app,
        input,
    )
    .await?;
    Ok(envelope(
        StatusCode::OK,
        configuration_view(saved, &app.auth, &state.public_url),
    ))
}

/// The contract's `AvailableApp`.
#[derive(Serialize)]
struct AvailableView {
    app: AppView,
    configuration: ConfigurationSummary,
}

/// The catalog's apps and the workspace's own, each only if ready in the
/// workspace's environment: never another environment's configuration.
async fn list_available_apps(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let (organization_id, workspace) = (access.access.organization_id, &access.workspace);
    let catalog = state
        .apps
        .list_organization_apps(organization_id)
        .await
        .map_err(app_failure)?;
    let catalog_configurations = state
        .apps
        .list_organization_configurations(organization_id)
        .await
        .map_err(app_failure)?;
    let own = state
        .apps
        .list_workspace_apps(organization_id, workspace.id)
        .await
        .map_err(app_failure)?;
    let own_configurations = state
        .apps
        .list_workspace_configurations(organization_id, workspace.id)
        .await
        .map_err(app_failure)?;
    let installations = installation_counts(&state, &access.access).await?;
    let ready = |app: &App, configurations: &[Configuration]| {
        configurations
            .iter()
            .find(|configuration| configuration.app_id == app.id && configuration.environment == workspace.environment)
            .filter(|configuration| configuration.status(&app.auth) == ConfigurationStatus::Ready)
            .map(|configuration| ConfigurationSummary::of(configuration, &app.auth))
    };
    let mut available: Vec<(String, AvailableView)> = Vec::new();
    for app in catalog {
        if let Some(configuration) = ready(&app, &catalog_configurations) {
            let name = app.name.to_ascii_lowercase();
            let app = view(
                app,
                &catalog_configurations,
                &catalog_environments(&access.access),
                &installations,
            );
            available.push((name, AvailableView { app, configuration }));
        }
    }
    for app in own {
        if let Some(configuration) = ready(&app, &own_configurations) {
            let name = app.name.to_ascii_lowercase();
            let app = view(app, &own_configurations, &[workspace.environment], &installations);
            available.push((name, AvailableView { app, configuration }));
        }
    }
    available.sort_by(|a, b| a.0.cmp(&b.0));
    let views: Vec<AvailableView> = available.into_iter().map(|(_, view)| view).collect();
    Ok(envelope(StatusCode::OK, views))
}
