//! `Idempotency-Key` for every `POST` (decision 0016): one middleware, so no
//! handler, service or table knows about it.
//!
//! - The caller is resolved first, exactly as the handler will resolve it, so
//!   `401` and `404` are never replayed past their checks; then a malformed
//!   key answers `400 invalid_idempotency_key`, before the route's own checks.
//! - A key belongs to that caller: the organization, the workspace on
//!   workspace routes, the environment and the credential. Creating an
//!   organization is the user's own key space. Hosted Connect holds no
//!   credential: its key belongs to the session its link names.
//! - The same key and request replays the saved status and body; another
//!   request answers `422`, one still in flight `409`.
//! - `5xx` saves nothing. A route that shows a secret once saves that it
//!   completed, never its body: a retry answers `409 idempotency_key_completed`.

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{FromRequestParts, MatchedPath, RawPathParams, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::idempotency::{Begun, HEADER, IdempotencyCaller, IdempotencyScope, valid_key};
use zupflow_core::time::Timestamp;

use crate::auth::{Access, Actor, AppState, Caller, WorkspaceAccess};
use crate::http::Failure;

/// Routes whose response shows a secret once, as the contract marks them
/// (`x-zupflow-shows-secret-once`); a test holds the two lists equal.
pub const SHOWS_SECRET_ONCE: [&str; 4] = [
    "/v1/organizations/{org}/api-keys",
    "/v1/organizations/{org}/webhooks",
    "/v1/workspaces/{ws}/connect-sessions",
    "/v1/connect/sessions/{reference}/authorize",
];

/// The same limit the routes read bodies with.
const MAX_BODY_BYTES: usize = 64 << 10;

/// Responses are small JSON; anything bigger is a bug, not something to save.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

fn unavailable() -> Response {
    Failure(ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    ))
    .into_response()
}

/// `POST` routes an API key can't call, as the contract lists their callers
/// (`x-zupflow-callers` without `api_key`); a test holds the two equal. A key
/// on one passes straight through, so the route answers `403 console_only`
/// whatever key or organization the request names.
pub const NOT_FOR_API_KEYS: [&str; 7] = [
    "/v1/organizations",
    "/v1/organizations/{org}/members",
    "/v1/organizations/{org}/api-keys",
    "/v1/organizations/{org}/api-keys/{key}/revoke",
    "/v1/connect/sessions/{reference}/credentials",
    "/v1/connect/sessions/{reference}/authorize",
    "/v1/connect/sessions/{reference}/cancel",
];

/// Who is calling, in the environment they act in: a member on an
/// organization route acts in none.
fn caller_of(access: &Access, environment: Option<Environment>) -> IdempotencyCaller {
    match &access.actor {
        Actor::Member(member) => IdempotencyCaller::Member {
            user_id: member.user.id,
            environment,
        },
        Actor::ApiKey(key) => IdempotencyCaller::ApiKey {
            api_key_id: key.id,
            environment: key.environment,
        },
    }
}

/// Whose key it is, resolved the way the handler resolves its caller. `None`
/// when idempotency doesn't apply: an API key on a route keys can't call,
/// which the route refuses with `403 console_only`. A `POST` route this doesn't recognise fails
/// closed: a key is never kept without an owner.
async fn scope_of(state: &AppState, parts: &mut Parts, template: &str) -> Result<Option<IdempotencyScope>, Failure> {
    let params = RawPathParams::from_request_parts(parts, state)
        .await
        .map_err(|_| Failure(ApiError::internal()))?;
    let param = |name: &str| {
        params
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_owned())
    };
    // Hosted Connect holds no credential: its link's reference names a
    // session, and the key is that session's, in its workspace. An unknown
    // reference is the route's own 404, before anything is kept.
    if let (true, Some(reference)) = (
        template.starts_with("/v1/connect/sessions/{reference}"),
        param("reference"),
    ) {
        let session = crate::connect::attributed(state, &reference).await?;
        return Ok(Some(IdempotencyScope::Organization {
            organization_id: session.organization_id,
            workspace_id: Some(session.workspace_id),
            caller: IdempotencyCaller::ConnectSession {
                session_id: session.id,
                environment: session.environment,
            },
        }));
    }
    let caller = Caller::from_request_parts(parts, state).await?;
    if matches!(caller, Caller::ApiKey(_)) && NOT_FOR_API_KEYS.contains(&template) {
        return Ok(None);
    }
    if template == "/v1/organizations" {
        return Ok(match caller {
            Caller::Console(user) => Some(IdempotencyScope::User { user_id: user.id }),
            Caller::ApiKey(_) => None,
        });
    }
    if let (true, Some(workspace)) = (template.starts_with("/v1/workspaces/{ws}"), param("ws")) {
        let access = WorkspaceAccess::resolve(state, caller, &parts.headers, &workspace).await?;
        return Ok(Some(IdempotencyScope::Organization {
            organization_id: access.access.organization_id,
            workspace_id: Some(access.workspace.id),
            caller: caller_of(&access.access, Some(access.workspace.environment)),
        }));
    }
    if let (true, Some(organization)) = (template.starts_with("/v1/organizations/{org}"), param("org")) {
        let access = Access::organization(state, caller, &organization).await?;
        return Ok(Some(IdempotencyScope::Organization {
            organization_id: access.organization_id,
            workspace_id: None,
            caller: caller_of(&access, None),
        }));
    }
    tracing::error!(event = "idempotency.unknown_scope", template);
    Err(ApiError::internal().into())
}

fn replay(status: u16, body: Option<Vec<u8>>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match body {
        Some(body) if !body.is_empty() && status != StatusCode::NO_CONTENT => {
            let mut response = (status, Bytes::from(body)).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            response
        }
        _ => status.into_response(),
    }
}

pub async fn idempotency(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request.method() != Method::POST {
        return next.run(request).await;
    }
    let Some(key) = request.headers().get(HEADER).cloned() else {
        return next.run(request).await;
    };
    let Some(template) = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
    else {
        return next.run(request).await;
    };
    let (mut parts, body) = request.into_parts();
    let scope = match scope_of(&state, &mut parts, &template).await {
        Ok(Some(scope)) => scope,
        Ok(None) => return next.run(Request::from_parts(parts, body)).await,
        Err(failure) => return failure.into_response(),
    };
    let key = match key.to_str() {
        Ok(key) if valid_key(key) => key.to_owned(),
        _ => {
            let mut error = ApiError::new(
                400,
                "invalid_idempotency_key",
                format!("{HEADER} must be 1–255 printable ASCII characters."),
            );
            error.fields = Some([(HEADER.to_owned(), "1–255 printable ASCII characters.".to_owned())].into());
            return Failure(error).into_response();
        }
    };
    let Ok(bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return Failure(ApiError::new(413, "body_too_large", "The request body is too large.")).into_response();
    };
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), |path| path.as_str().to_owned());
    let fingerprint = state.fingerprinter.fingerprint(&template, &path_and_query, &bytes);

    let lock = match state
        .idempotency
        .begin(&scope, &key, &fingerprint, Timestamp::now())
        .await
    {
        Err(_) => return unavailable(),
        Ok(Begun::Reused) => {
            return Failure(ApiError::new(
                422,
                "idempotency_key_reused",
                "This idempotency key was already used for a different request. Use a new key for a new request.",
            ))
            .into_response();
        }
        Ok(Begun::InProgress) => {
            return Failure(ApiError::new(
                409,
                "idempotency_request_in_progress",
                "A request with this idempotency key is still in progress. Retry in a moment.",
            ))
            .into_response();
        }
        Ok(Begun::Replay { status, body: None }) if status != 204 => {
            return Failure(ApiError::new(
                409,
                "idempotency_key_completed",
                "This request already completed, and its response showed a secret that is never shown twice. \
                 Find the resource in its list, or start again with a new key.",
            ))
            .into_response();
        }
        Ok(Begun::Replay { status, body }) => return replay(status, body),
        Ok(Begun::Run(lock)) => lock,
    };

    // The handler and the save run as one task: if the client goes away
    // mid-request, the request's future is dropped but the task finishes, so
    // a handler that committed always has its answer saved.
    let request = Request::from_parts(parts, Body::from(bytes));
    let saving = tokio::spawn(async move { run_and_save(state, scope, key, lock, template, request, next).await });
    saving
        .await
        .unwrap_or_else(|_| Failure(ApiError::internal()).into_response())
}

async fn run_and_save(
    state: AppState,
    scope: IdempotencyScope,
    key: String,
    lock: zupflow_core::idempotency::LockToken,
    template: String,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    let (response_parts, response_body) = response.into_parts();
    let Ok(response_bytes) = to_bytes(response_body, MAX_RESPONSE_BYTES).await else {
        let _ = state.idempotency.abandon(&scope, &key, &lock).await;
        tracing::error!(event = "idempotency.unreadable_response", template);
        return Failure(ApiError::internal()).into_response();
    };
    let status = response_parts.status;
    let saved = if status.is_server_error() {
        // Nothing is saved for a server error, so a retry runs again.
        state.idempotency.abandon(&scope, &key, &lock).await
    } else {
        // An error carries no secret; a success from a secret-once route is never kept.
        let body =
            (!(status.is_success() && SHOWS_SECRET_ONCE.contains(&template.as_str()))).then(|| response_bytes.to_vec());
        state
            .idempotency
            .complete(&scope, &key, &lock, status.as_u16(), body)
            .await
    };
    if saved.is_err() {
        // The request already ran; its answer goes out. A retry may run it again.
        tracing::error!(event = "idempotency.save_failed", template);
    }
    Response::from_parts(response_parts, Body::from(response_bytes))
}
