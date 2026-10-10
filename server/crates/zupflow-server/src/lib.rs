//! The Zupflow API server: HTTP routing over the domain services.
//!
//! [`app`] builds the router over an [`AppState`]; `main.rs` adds
//! configuration, logging, the adapters and the listener. Tests drive [`app`]
//! directly.

pub mod api_keys;
pub mod app_configurations;
pub mod apps;
pub mod auth;
pub mod auth_templates;
pub mod config;
pub mod connect;
pub mod guard;
pub mod http;
pub mod idempotency;
pub mod input;
pub mod installations;
pub mod organizations;
pub mod workspaces;

use axum::Router;
use axum::routing::get;

pub use auth::AppState;

pub use guard::RequestGuard;

/// The contract's operations this server serves so far, by `operationId`. The
/// shared contract suite skips, by name, any case that needs another one; a
/// test holds this list equal to the routes the router actually answers.
pub const SERVED_OPERATIONS: [&str; 43] = [
    "getViewer",
    "createOrganization",
    "getOrganization",
    "updateOrganization",
    "listMembers",
    "inviteMember",
    "updateMember",
    "removeMember",
    "listWorkspaces",
    "createWorkspace",
    "getWorkspace",
    "updateWorkspace",
    "deleteWorkspace",
    "listApiKeys",
    "createApiKey",
    "revokeApiKey",
    "listOrganizationApps",
    "createOrganizationApp",
    "getOrganizationApp",
    "updateOrganizationApp",
    "listWorkspaceApps",
    "createWorkspaceApp",
    "getWorkspaceApp",
    "updateWorkspaceApp",
    "getOrganizationAppConfiguration",
    "updateOrganizationAppConfiguration",
    "getWorkspaceAppConfiguration",
    "updateWorkspaceAppConfiguration",
    "listAvailableApps",
    "listOrganizationAppInstallations",
    "listInstallations",
    "getInstallation",
    "updateInstallation",
    "disconnectInstallation",
    "deleteInstallation",
    "getConnectSettings",
    "updateConnectSettings",
    "listConnectSessions",
    "createConnectSession",
    "getConnectSession",
    "getHostedConnect",
    "cancelConnect",
    "submitConnectCredentials",
];

/// The whole HTTP surface. Every response under `/v1` is the envelope; the
/// health routes are for the deployment, outside the API. `guard` refuses
/// requests a browser was tricked into sending, before any route runs.
pub fn app(state: AppState, guard: RequestGuard) -> Router {
    Router::new()
        .route("/health", get(http::health))
        .route("/ready", get(http::ready))
        .merge(organizations::routes())
        .merge(api_keys::routes())
        .merge(workspaces::routes())
        .merge(apps::routes())
        .merge(app_configurations::routes())
        .merge(installations::routes())
        .merge(connect::routes())
        // Matched routes only, after routing, so the middleware knows the route's template.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            idempotency::idempotency,
        ))
        .fallback(http::route_not_found)
        .method_not_allowed_fallback(http::method_not_allowed)
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(guard, guard::guard))
}
