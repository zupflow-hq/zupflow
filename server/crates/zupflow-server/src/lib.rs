//! The Zupflow API server: HTTP routing over the domain services.
//!
//! [`app`] builds the router; `main.rs` adds configuration, logging and the
//! listener. Tests drive [`app`] directly.

pub mod config;
pub mod http;

use axum::Router;
use axum::routing::get;

/// The whole HTTP surface. Every response under `/v1` is the envelope; the
/// health routes are for the deployment, outside the API.
pub fn app() -> Router {
    Router::new()
        .route("/health", get(http::health))
        .route("/ready", get(http::ready))
        .fallback(http::route_not_found)
        .method_not_allowed_fallback(http::method_not_allowed)
}
