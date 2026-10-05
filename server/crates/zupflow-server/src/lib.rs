//! The Zupflow API server: HTTP routing over the domain services.
//!
//! [`app`] builds the router over an [`AppState`]; `main.rs` adds
//! configuration, logging, the adapters and the listener. Tests drive [`app`]
//! directly.

pub mod auth;
pub mod config;
pub mod guard;
pub mod http;
pub mod input;
pub mod organizations;

use axum::Router;
use axum::routing::get;

pub use auth::AppState;
pub use guard::RequestGuard;

/// The whole HTTP surface. Every response under `/v1` is the envelope; the
/// health routes are for the deployment, outside the API. `guard` refuses
/// requests a browser was tricked into sending, before any route runs.
pub fn app(state: AppState, guard: RequestGuard) -> Router {
    Router::new()
        .route("/health", get(http::health))
        .route("/ready", get(http::ready))
        .merge(organizations::routes())
        .fallback(http::route_not_found)
        .method_not_allowed_fallback(http::method_not_allowed)
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(guard, guard::guard))
}
