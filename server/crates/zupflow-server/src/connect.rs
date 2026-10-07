//! Connect (decision 0005):
//! - an organization's allowlists, at `/v1/organizations/{org}/connect-settings/{env}`;
//! - a workspace's sessions, at `/v1/workspaces/{ws}/connect-sessions`;
//! - the hosted page's view of one session, at `/v1/connect/sessions/{reference}`.
//!
//! The hosted routes carry no credential: the link's reference is the
//! authority. A request's reference is hashed and the session found by that
//! hash alone, the store's one cross-tenant lookup; everything after it uses
//! the session's own organization and workspace. A bad, unknown or reused
//! reference answers the same `404` or `409`, never anything of a session.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::Value;
use zupflow_core::apps::{App, AuthTemplate, ConfigurationStatus};
use zupflow_core::connect::{
    ConnectError, ConnectIntent, ConnectSession, ConnectSessionStatus, ConnectSettings, NewConnectSession, Reference,
    ReferenceHash,
};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AppId, ConnectSessionId, InstallationId, WorkspaceId};
use zupflow_core::installations::{ConnectedAccount, InstalledApp};
use zupflow_core::organizations::MemberRole;
use zupflow_core::time::Timestamp;

use crate::apps::app_failure;
use crate::auth::{Access, Actor, AppState, Caller, WorkspaceAccess};
use crate::http::{Failure, envelope};
use crate::input::{Body, Fields, Object};
use crate::installations::{InstallationAppView, installation_app_view};
use crate::workspaces::environment_mismatch;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/organizations/{org}/connect-settings/{env}",
            get(get_settings).put(update_settings),
        )
        .route(
            "/v1/workspaces/{ws}/connect-sessions",
            get(list_sessions).post(create_session),
        )
        .route("/v1/workspaces/{ws}/connect-sessions/{session}", get(get_session))
        .route("/v1/connect/sessions/{reference}", get(get_hosted))
        .route("/v1/connect/sessions/{reference}/cancel", post(cancel_hosted))
}

const LIST_MAX: usize = 20;
/// The longest return URL or origin a session can carry.
const ENTRY_MAX: usize = 2000;

/// Settings refusals, with the mock's message.
fn entries_invalid(fields: Fields) -> Result<(), Failure> {
    fields.finish().map_err(|Failure(mut error)| {
        error.message = "Some entries are invalid.".to_owned();
        Failure(error)
    })
}
const LABEL_MAX: usize = 40;

pub(crate) fn connect_failure(error: ConnectError) -> Failure {
    match error {
        ConnectError::NotFound => ApiError::not_found("Connect session").into(),
        ConnectError::NotPending => not_pending(ConnectSessionStatus::Completed),
        // The store has logged the cause.
        ConnectError::Unavailable => ApiError::new(
            503,
            "unavailable",
            "The service is unavailable right now. Try again shortly.",
        )
        .into(),
    }
}

fn not_pending(status: ConnectSessionStatus) -> Failure {
    ApiError::new(
        409,
        "session_not_pending",
        if status == ConnectSessionStatus::Expired {
            "This Connect link has expired. Start again from the app."
        } else {
            "This Connect session was already used. Start again from the app."
        },
    )
    .into()
}

// --- Settings ---------------------------------------------------------------

/// The contract's `ConnectSettings`.
#[derive(Serialize)]
struct SettingsView {
    environment: Environment,
    return_urls: Vec<String>,
    allowed_origins: Vec<String>,
    updated_at: Option<Timestamp>,
}

fn settings_view(environment: Environment, settings: ConnectSettings) -> SettingsView {
    SettingsView {
        environment,
        return_urls: settings.return_urls,
        allowed_origins: settings.allowed_origins,
        updated_at: settings.updated_at,
    }
}

/// The environment in the path; a key acts in its own only.
fn environment(access: &Access, env: &str) -> Result<Environment, Failure> {
    let environment = Environment::parse(env).ok_or_else(|| Failure(ApiError::not_found("Environment")))?;
    if access.environment().is_some_and(|own| own != environment) {
        return Err(environment_mismatch());
    }
    Ok(environment)
}

/// `http://localhost` is allowed only in Sandbox; everything else is `https`.
fn secure_enough(url: &url::Url, environment: Environment) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http" && environment == Environment::Sandbox && url.host_str() == Some("localhost"))
}

/// A list field of strings, at most 20, each at most 2,000 characters.
fn strings(body: &Object, field: &str, fields: &mut Fields) -> Option<Vec<String>> {
    match body.0.get(field) {
        Some(Value::Array(items)) if items.len() <= LIST_MAX => {
            let mut strings = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                match item.as_str() {
                    Some(value) if value.chars().count() <= ENTRY_MAX => strings.push(value.to_owned()),
                    Some(_) => fields.invalid(&format!("{field}.{index}"), "Use 2,000 characters or fewer."),
                    None => fields.invalid(&format!("{field}.{index}"), "Must be a string."),
                }
            }
            (strings.len() == items.len()).then_some(strings)
        }
        Some(Value::Array(_)) => {
            fields.invalid(field, "Use 20 entries or fewer.");
            None
        }
        Some(_) => {
            fields.invalid(field, "Must be a list.");
            None
        }
        None => {
            fields.invalid(field, "Required.");
            None
        }
    }
}

async fn get_settings(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, env)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let environment = environment(&access, &env)?;
    let settings = state
        .connect
        .find_settings(access.organization_id, environment)
        .await
        .map_err(connect_failure)?;
    Ok(envelope(StatusCode::OK, settings_view(environment, settings)))
}

async fn update_settings(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, env)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "change Connect settings")?;
    let environment = environment(&access, &env)?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let return_urls = strings(&body, "return_urls", &mut fields);
    let allowed_origins = strings(&body, "allowed_origins", &mut fields);
    entries_invalid(fields)?;
    let (Some(return_urls), Some(allowed_origins)) = (return_urls, allowed_origins) else {
        return Err(ApiError::internal().into());
    };

    let mut fields = Fields::default();
    for (index, value) in return_urls.iter().enumerate() {
        let field = format!("return_urls.{index}");
        if value.contains('*') {
            fields.invalid(&field, "Wildcards aren't allowed. List each URL exactly.");
            continue;
        }
        match url::Url::parse(value) {
            Err(_) => fields.invalid(&field, "Enter a full URL."),
            // As the mock: a fragment is named before an insecure scheme.
            Ok(url) if url.fragment().is_some_and(|fragment| !fragment.is_empty()) => {
                fields.invalid(&field, "Remove the #fragment.");
            }
            Ok(url) if !secure_enough(&url, environment) => {
                fields.invalid(&field, "Use https:// (http://localhost is allowed in Sandbox).");
            }
            Ok(_) => {}
        }
    }
    let mut origins = Vec::with_capacity(allowed_origins.len());
    for (index, value) in allowed_origins.iter().enumerate() {
        let field = format!("allowed_origins.{index}");
        let trimmed = value.strip_suffix('/').unwrap_or(value);
        match url::Url::parse(value) {
            Err(_) => fields.invalid(&field, "Enter an origin like https://app.example.com."),
            Ok(url) if url.origin().ascii_serialization() != trimmed => {
                fields.invalid(&field, "Use an origin only: scheme, host and port.");
            }
            Ok(url) if !secure_enough(&url, environment) => {
                fields.invalid(&field, "Use https:// (http://localhost is allowed in Sandbox).");
            }
            Ok(_) => origins.push(trimmed.to_owned()),
        }
    }
    entries_invalid(fields)?;
    let mut urls: Vec<String> = Vec::with_capacity(return_urls.len());
    for url in return_urls {
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    let mut unique_origins: Vec<String> = Vec::with_capacity(origins.len());
    for origin in origins {
        if !unique_origins.contains(&origin) {
            unique_origins.push(origin);
        }
    }
    let settings = state
        .connect
        .save_settings(access.organization_id, environment, &urls, &unique_origins)
        .await
        .map_err(connect_failure)?;
    Ok(envelope(StatusCode::OK, settings_view(environment, settings)))
}

// --- Sessions ---------------------------------------------------------------

/// The contract's `ConnectSession`, and with `url` its `CreatedConnectSession`.
#[derive(Serialize)]
struct SessionView {
    id: ConnectSessionId,
    workspace_id: WorkspaceId,
    environment: Environment,
    app: InstallationAppView,
    intent: ConnectIntent,
    installation_id: Option<InstallationId>,
    label: String,
    status: ConnectSessionStatus,
    return_url: Option<String>,
    created_at: Timestamp,
    expires_at: Timestamp,
    completed_at: Option<Timestamp>,
    failure_code: Option<String>,
    /// Only on the hosted page's read, so it can render its form.
    auth: Option<AuthTemplate>,
    requested_scopes: Vec<String>,
    expected_account: Option<ConnectedAccount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

/// A session's app, read in the session's own scope.
async fn app_of(state: &AppState, session: &ConnectSession) -> Result<App, Failure> {
    let app = match session.app {
        InstalledApp::Catalog(id) => state.apps.find_organization_app(session.organization_id, id).await,
        InstalledApp::Workspace(id) => {
            state
                .apps
                .find_workspace_app(session.organization_id, session.workspace_id, id)
                .await
        }
    }
    .map_err(app_failure)?;
    app.ok_or_else(|| {
        tracing::error!(event = "connect.app_missing", session_id = %session.id);
        Failure(ApiError::internal())
    })
}

/// For a reconnect, the account it must use again, read in the session's scope.
async fn expected_account(state: &AppState, session: &ConnectSession) -> Result<Option<ConnectedAccount>, Failure> {
    let Some(installation_id) = session.installation_id else {
        return Ok(None);
    };
    let installation = state
        .installations
        .find_installation(session.organization_id, session.workspace_id, installation_id)
        .await
        .map_err(|_| {
            Failure(ApiError::new(
                503,
                "unavailable",
                "The service is unavailable right now. Try again shortly.",
            ))
        })?;
    Ok(installation.and_then(|installation| installation.connection.map(|connection| connection.account)))
}

async fn session_view(
    state: &AppState,
    session: ConnectSession,
    hosted: bool,
    url: Option<String>,
) -> Result<SessionView, Failure> {
    let app = app_of(state, &session).await?;
    let expected_account = expected_account(state, &session).await?;
    Ok(SessionView {
        id: session.id,
        workspace_id: session.workspace_id,
        environment: session.environment,
        app: installation_app_view(&app, session.app),
        intent: session.intent,
        installation_id: session.installation_id,
        label: session.label.clone(),
        status: session.status_at(Timestamp::now()),
        return_url: session.return_url.clone(),
        created_at: session.created_at,
        expires_at: session.expires_at,
        completed_at: session.completed_at,
        failure_code: session.failure_code.clone(),
        auth: hosted.then(|| app.auth.clone()),
        requested_scopes: session.requested_scopes.clone(),
        expected_account,
        url,
    })
}

fn session_id(session: &str) -> Result<ConnectSessionId, Failure> {
    ConnectSessionId::parse(session).map_err(|_| ApiError::not_found("Connect session").into())
}

async fn list_sessions(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let sessions = state
        .connect
        .list_sessions(access.access.organization_id, access.workspace.id)
        .await
        .map_err(connect_failure)?;
    let mut views = Vec::with_capacity(sessions.len());
    for session in sessions {
        views.push(session_view(&state, session, false, None).await?);
    }
    Ok(envelope(StatusCode::OK, views))
}

async fn get_session(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path((ws, session)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let session = state
        .connect
        .find_session(
            access.access.organization_id,
            access.workspace.id,
            session_id(&session)?,
        )
        .await
        .map_err(connect_failure)?
        .ok_or_else(|| connect_failure(ConnectError::NotFound))?;
    Ok(envelope(
        StatusCode::OK,
        session_view(&state, session, false, None).await?,
    ))
}

/// The app the session is for: the organization's catalog app or this
/// workspace's own, ready in the workspace's environment. `404 App` when
/// neither exists here; `422 configuration_not_ready` when it isn't ready.
async fn ready_app(
    state: &AppState,
    access: &WorkspaceAccess,
    app_id: &str,
) -> Result<(InstalledApp, u32, Vec<String>), Failure> {
    let not_found = || Failure(ApiError::not_found("App"));
    let app_id = AppId::parse(app_id).map_err(|_| not_found())?;
    let (organization_id, workspace) = (access.access.organization_id, &access.workspace);
    let (app, installed, configuration) = if let Some(app) = state
        .apps
        .find_organization_app(organization_id, app_id)
        .await
        .map_err(app_failure)?
    {
        let configuration = state
            .apps
            .find_organization_configuration(organization_id, app_id, workspace.environment)
            .await
            .map_err(app_failure)?;
        (app, InstalledApp::Catalog(app_id), configuration)
    } else if let Some(app) = state
        .apps
        .find_workspace_app(organization_id, workspace.id, app_id)
        .await
        .map_err(app_failure)?
    {
        let configuration = state
            .apps
            .find_workspace_configuration(organization_id, workspace.id, app_id)
            .await
            .map_err(app_failure)?;
        (app, InstalledApp::Workspace(app_id), configuration)
    } else {
        return Err(not_found());
    };
    let environment = workspace.environment.as_str();
    match configuration {
        Some(configuration) if configuration.status(&app.auth) == ConfigurationStatus::Ready => {
            let scopes = match app.auth {
                AuthTemplate::OAuth2 { .. } => configuration.oauth_scopes.clone(),
                AuthTemplate::Token { .. } => Vec::new(),
            };
            Ok((installed, configuration.version, scopes))
        }
        _ => Err(ApiError::new(
            422,
            "configuration_not_ready",
            format!(
                "{} isn't configured for {environment} yet. Finish its {environment} configuration first.",
                app.name
            ),
        )
        .into()),
    }
}

async fn create_session(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(MemberRole::is_developer, "start Connect sessions")?;
    // In Production the Console only observes: connecting a customer's
    // account there is the customer's backend's job, with an API key.
    if matches!(access.access.actor, Actor::Member(_)) && access.workspace.environment == Environment::Production {
        return Err(ApiError::new(
            403,
            "console_read_only_in_production",
            "The Console can't connect accounts in Production. Your product does that through the API.",
        )
        .into());
    }
    let body = body.object()?;
    let mut fields = Fields::default();
    let app_id = body.string("app_id", &mut fields);
    let intent = body.string("intent", &mut fields).and_then(|value| {
        let intent = ConnectIntent::parse(&value);
        if intent.is_none() {
            fields.invalid("intent", "Choose create or reconnect.");
        }
        intent
    });
    let label = body.string("label", &mut fields).and_then(|label| {
        let label = label.trim().to_owned();
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
    let installation_id = match body.0.get("installation_id") {
        None => {
            fields.invalid("installation_id", "Required; send null for none.");
            None
        }
        Some(Value::Null) => Some(None),
        Some(Value::String(value)) => Some(Some(value.clone())),
        Some(_) => {
            fields.invalid("installation_id", "Must be a string or null.");
            None
        }
    };
    let return_url = match body.0.get("return_url") {
        None => {
            fields.invalid("return_url", "Required; send null for none.");
            None
        }
        Some(Value::Null) => Some(None),
        Some(Value::String(value)) if url::Url::parse(value).is_ok() => Some(Some(value.clone())),
        Some(Value::String(_)) => {
            fields.invalid("return_url", "Enter a full URL.");
            None
        }
        Some(_) => {
            fields.invalid("return_url", "Must be a URL or null.");
            None
        }
    };
    fields.finish()?;
    let (Some(app_id), Some(intent), Some(label), Some(installation_id), Some(return_url)) =
        (app_id, intent, label, installation_id, return_url)
    else {
        return Err(ApiError::internal().into());
    };

    let (installed, configuration_version, requested_scopes) = ready_app(&state, &access, &app_id).await?;
    let (organization_id, workspace) = (access.access.organization_id, &access.workspace);
    let (installation_id, label) = match (intent, installation_id) {
        (ConnectIntent::Create, Some(_)) => {
            // A new installation has no existing one to point at; never store an unchecked reference.
            let mut error = ApiError::new(
                400,
                "validation_failed",
                "A new installation can't name an existing one.",
            );
            error.fields = Some(
                [(
                    "installation_id".to_owned(),
                    "Leave empty unless you're reconnecting.".to_owned(),
                )]
                .into(),
            );
            return Err(error.into());
        }
        (ConnectIntent::Create, None) => (None, label),
        (ConnectIntent::Reconnect, None) => {
            let mut error = ApiError::new(400, "validation_failed", "Choose the installation to reconnect.");
            error.fields = Some([("installation_id".to_owned(), "Required to reconnect.".to_owned())].into());
            return Err(error.into());
        }
        (ConnectIntent::Reconnect, Some(installation_id)) => {
            let not_found = || Failure(ApiError::not_found("Installation"));
            let installation_id = InstallationId::parse(&installation_id).map_err(|_| not_found())?;
            let existing = state
                .installations
                .find_installation(organization_id, workspace.id, installation_id)
                .await
                .map_err(|_| {
                    Failure(ApiError::new(
                        503,
                        "unavailable",
                        "The service is unavailable right now. Try again shortly.",
                    ))
                })?
                .ok_or_else(not_found)?;
            if existing.app != installed {
                return Err(ApiError::new(422, "app_mismatch", "That installation belongs to a different app.").into());
            }
            (Some(installation_id), existing.label)
        }
    };
    if let Some(url) = &return_url {
        let settings = state
            .connect
            .find_settings(organization_id, workspace.environment)
            .await
            .map_err(connect_failure)?;
        if !settings.return_urls.contains(url) {
            let mut error = ApiError::new(422, "return_url_not_allowed", "That return URL isn't on the allowlist.");
            error.fields = Some(
                [(
                    "return_url".to_owned(),
                    format!("Add it to the {} return URLs first.", workspace.environment.as_str()),
                )]
                .into(),
            );
            return Err(error.into());
        }
    }
    let reference = Reference::generate().map_err(connect_failure)?;
    let session = state
        .connect
        .create_session(
            organization_id,
            workspace.id,
            ConnectSessionId::generate(),
            &NewConnectSession {
                environment: workspace.environment,
                app: installed,
                configuration_version,
                intent,
                installation_id,
                label,
                return_url,
                requested_scopes,
                reference_hash: reference.hash(),
            },
            Timestamp::now(),
        )
        .await
        .map_err(connect_failure)?;
    // The link is shown once, here: its reference authorizes the hosted page.
    let url = format!("{}/connect/{}", state.public_url, reference.expose());
    Ok(envelope(
        StatusCode::CREATED,
        session_view(&state, session, false, Some(url)).await?,
    ))
}

// --- Hosted Connect ---------------------------------------------------------

/// The session a link's reference names. Anything else is the same `404`.
pub(crate) async fn attributed(state: &AppState, reference: &str) -> Result<ConnectSession, Failure> {
    if reference.is_empty() || reference.len() > 64 {
        return Err(connect_failure(ConnectError::NotFound));
    }
    state
        .connect
        .attribute_session_by_reference(&ReferenceHash::of(reference))
        .await
        .map_err(connect_failure)?
        .ok_or_else(|| connect_failure(ConnectError::NotFound))
}

async fn get_hosted(State(state): State<AppState>, Path(reference): Path<String>) -> Result<Response, Failure> {
    let session = attributed(&state, &reference).await?;
    Ok(envelope(
        StatusCode::OK,
        session_view(&state, session, true, None).await?,
    ))
}

async fn cancel_hosted(State(state): State<AppState>, Path(reference): Path<String>) -> Result<Response, Failure> {
    let session = attributed(&state, &reference).await?;
    let status = session.status_at(Timestamp::now());
    if status != ConnectSessionStatus::Pending {
        return Err(not_pending(status));
    }
    let cancelled = state
        .connect
        .cancel_session(
            session.organization_id,
            session.workspace_id,
            session.id,
            Timestamp::now(),
        )
        .await
        .map_err(connect_failure)?;
    Ok(envelope(
        StatusCode::OK,
        session_view(&state, cancelled, false, None).await?,
    ))
}
