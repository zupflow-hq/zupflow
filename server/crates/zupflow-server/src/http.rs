//! The envelope, error responses, and the routes outside the API.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use zupflow_core::error::ApiError;

/// A success: `{ data, meta: null, error: null }` with the given status.
pub fn envelope<T: Serialize>(status: StatusCode, data: T) -> Response {
    (status, Json(json!({ "data": data, "meta": null, "error": null }))).into_response()
}

/// An [`ApiError`] as an HTTP response: the envelope with `data` and `meta`
/// null. A newtype, because the error type lives in the core crate.
#[derive(Debug)]
pub struct Failure(pub ApiError);

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.status)
            .ok()
            // An error envelope only ever goes out with an error status.
            .filter(|status| status.is_client_error() || status.is_server_error())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            Json(json!({ "data": null, "meta": null, "error": self.0.body() })),
        )
            .into_response()
    }
}

/// The process is up.
pub async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// The process can serve requests. Checks its dependencies once it has any.
pub async fn ready() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// Any path the API doesn't serve answers like a missing resource.
pub async fn route_not_found() -> Failure {
    ApiError::new(404, "not_found", "There's nothing at this path.").into()
}

pub async fn method_not_allowed() -> Failure {
    ApiError::new(405, "method_not_allowed", "This path doesn't accept that method.").into()
}
