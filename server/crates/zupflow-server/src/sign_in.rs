//! How the Console signs in (decision 0020): `local`, or `oidc` with the
//! issuer and the Console's client ID. Public: the route takes no credential,
//! and answers only what any browser needs to start signing in.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde::Serialize;

use crate::auth::AppState;
use crate::http::envelope;

/// What `GET /v1/sign-in-settings` answers, as the contract's `SignInSettings`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SignInSettings {
    /// No sign-in: every Console request is the built-in owner.
    Local,
    /// Sign in at an OpenID Connect provider and send its ID token.
    Oidc {
        /// The `iss` of the provider's ID tokens.
        issuer: String,
        /// The Console's client ID: the audience the server accepts.
        client_id: String,
    },
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/sign-in-settings", get(sign_in_settings))
}

/// Never reads the caller's credential: the answer is the deployment's, the
/// same for everyone.
async fn sign_in_settings(State(state): State<AppState>) -> Response {
    envelope(StatusCode::OK, &state.sign_in)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_the_contract_names_it() {
        assert_eq!(
            serde_json::to_value(SignInSettings::Local).ok(),
            Some(serde_json::json!({ "mode": "local" }))
        );
        assert_eq!(
            serde_json::to_value(SignInSettings::Oidc {
                issuer: "https://idp.example".to_owned(),
                client_id: "console".to_owned(),
            })
            .ok(),
            Some(serde_json::json!({ "mode": "oidc", "issuer": "https://idp.example", "client_id": "console" }))
        );
    }
}
