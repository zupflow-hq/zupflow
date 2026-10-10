//! API keys: listed, created and revoked from the Console. A key's secret is
//! in the response that creates it and nowhere else, ever.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Serialize;
use zupflow_core::api_keys::{ApiKey, ApiKeyError, NewApiKey, NewSecret};
use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::ApiKeyId;
use zupflow_core::organizations::MemberRole;

use crate::auth::{AppState, ConsoleUser, OrganizationAccess};
use crate::http::{Failure, envelope};
use crate::input::{Body, Fields};
use crate::organizations::checked_name;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/organizations/{org}/api-keys",
            get(list_api_keys).post(create_api_key),
        )
        .route("/v1/organizations/{org}/api-keys/{key}/revoke", post(revoke_api_key))
}

fn unavailable(_: ApiKeyError) -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

/// Developers manage Sandbox keys; Production keys need an admin or owner.
fn may_manage(access: &OrganizationAccess, environment: Environment, verb: &str) -> Result<(), Failure> {
    let developer = |role: MemberRole| role != MemberRole::Viewer;
    access.require(developer, &format!("{verb} API keys"))?;
    if environment == Environment::Production {
        access.require(MemberRole::is_admin, &format!("{verb} Production keys"))?;
    }
    Ok(())
}

async fn list_api_keys(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    let keys = state
        .api_keys
        .list_api_keys(access.organization.id)
        .await
        .map_err(unavailable)?;
    Ok(envelope(StatusCode::OK, keys))
}

/// The contract's `CreatedApiKey`.
#[derive(Serialize)]
struct CreatedApiKey<'a> {
    key: ApiKey,
    secret: &'a str,
}

async fn create_api_key(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(|role| role != MemberRole::Viewer, "create API keys")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let name = checked_name(body.string("name", &mut fields), &mut fields);
    let environment = body
        .string("environment", &mut fields)
        .and_then(|value| match Environment::parse(&value) {
            Some(environment) => Some(environment),
            None => {
                fields.invalid("environment", "Choose sandbox or production.");
                None
            }
        });
    fields.finish()?;
    let (Some(name), Some(environment)) = (name, environment) else {
        return Err(ApiError::internal().into());
    };
    may_manage(&access, environment, "create")?;

    let secret = NewSecret::generate(environment).map_err(unavailable)?;
    let key = state
        .api_keys
        .create_api_key(
            access.organization.id,
            ApiKeyId::generate(),
            &NewApiKey {
                environment,
                name,
                prefix: secret.prefix.clone(),
                last4: secret.last4.clone(),
                secret_hash: state.api_key_hasher.hash(&secret.secret),
                created_by: access.user.id,
            },
            &|key: &ApiKey| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::ApiKeyCreated,
                    AuditTarget::new(AuditObject::ApiKey(key.id), &key.name),
                    AuditPlace::Environment(key.environment),
                )
            },
        )
        .await
        .map_err(unavailable)?;
    Ok(envelope(
        StatusCode::CREATED,
        CreatedApiKey {
            key,
            secret: &secret.secret,
        },
    ))
}

async fn revoke_api_key(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path((org, key)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(|role| role != MemberRole::Viewer, "revoke API keys")?;
    let not_found = || Failure(ApiError::not_found("API key"));
    let key_id = ApiKeyId::parse(&key).map_err(|_| not_found())?;
    let key = state
        .api_keys
        .find_api_key(access.organization.id, key_id)
        .await
        .map_err(unavailable)?
        .ok_or_else(not_found)?;
    may_manage(&access, key.environment, "revoke")?;
    let revoked = state
        .api_keys
        .revoke_api_key(access.organization.id, key_id, &|key: &ApiKey| {
            AuditEntry::new(
                access.audit_actor(),
                AuditAction::ApiKeyRevoked,
                AuditTarget::new(AuditObject::ApiKey(key.id), &key.name),
                AuditPlace::Environment(key.environment),
            )
        })
        .await
        .map_err(|error| match error {
            ApiKeyError::NotFound => not_found(),
            ApiKeyError::AlreadyRevoked => ApiError::new(409, "already_revoked", "This key is already revoked.").into(),
            error => unavailable(error),
        })?;
    Ok(envelope(StatusCode::OK, revoked))
}
