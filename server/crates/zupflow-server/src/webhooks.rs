//! Webhook endpoints (decision 0017), at `/v1/organizations/{org}/webhooks…`:
//! listed by every member and the organization's API keys, and added,
//! changed, deleted and tested by owners, admins and API keys. An API key
//! reaches only its own environment's endpoints.
//!
//! An endpoint's signing secret is made here, sealed under the endpoint's
//! `webhook_signing_secret` context, and shown once, in the response that
//! creates it; responses show its last four characters only. The test
//! button sends a real, signed `webhook.test` event through the outbound path
//! and answers what came back; `503 unavailable` when Zupflow itself
//! couldn't send it (its store or key service), which the endpoint isn't
//! charged with.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use serde::Serialize;
use serde_json::{Value, json};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{WebhookEndpointId, WebhookEventId};
use zupflow_core::keys::{EncryptionContext, seal};
use zupflow_core::organizations::MemberRole;
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::{
    Attempt, DeliveryState, EndpointChanges, LastDelivery, NewEndpoint, NewWebhookEvent, SECRET_VERSION, TEST_EVENT,
    WebhookEndpoint, WebhookError, WebhookEventType, event_body, new_signing_secret,
};

use crate::app_configurations::check_public_https;
use crate::auth::{Access, AppState, Caller};
use crate::http::{Failure, envelope};
use crate::input::{Body, Fields, Object};
use crate::webhook_delivery::{self, Unsent};
use crate::workspaces::environment_mismatch;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/organizations/{org}/webhooks",
            get(list_webhooks).post(create_webhook),
        )
        .route(
            "/v1/organizations/{org}/webhooks/{hook}",
            patch(update_webhook).delete(delete_webhook),
        )
        .route("/v1/organizations/{org}/webhooks/{hook}/test", post(test_webhook))
}

/// The longest description, in characters.
const MAX_DESCRIPTION: usize = 200;

/// The longest URL, in characters.
const MAX_URL: usize = 2048;

fn unavailable(_: WebhookError) -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

fn not_found() -> Failure {
    ApiError::not_found("Webhook endpoint").into()
}

/// The endpoint a path names, in the caller's organization and, for an API
/// key, its environment; anything else is the same `404`.
async fn find(state: &AppState, access: &Access, hook: &str) -> Result<WebhookEndpoint, Failure> {
    let endpoint_id = WebhookEndpointId::parse(hook).map_err(|_| not_found())?;
    state
        .webhooks
        .find_endpoint(access.organization_id, endpoint_id)
        .await
        .map_err(unavailable)?
        .filter(|endpoint| access.environment().is_none_or(|own| own == endpoint.environment))
        .ok_or_else(not_found)
}

fn checked_url(url: Option<String>, fields: &mut Fields) -> Option<String> {
    let url = url?.trim().to_owned();
    if url.chars().count() > MAX_URL {
        fields.invalid("url", "Use 2,048 characters or fewer.");
        return None;
    }
    Some(url)
}

fn checked_description(description: Option<String>, fields: &mut Fields) -> Option<String> {
    let description = description?.trim().to_owned();
    if description.chars().count() > MAX_DESCRIPTION {
        fields.invalid("description", "Use 200 characters or fewer.");
        return None;
    }
    Some(description)
}

/// The event types an endpoint subscribes to: at least one, each a type an
/// endpoint can subscribe to (never `webhook.test`), each once.
fn checked_events(body: &Object, fields: &mut Fields, required: bool) -> Option<Vec<WebhookEventType>> {
    let value = match body.0.get("events") {
        None => {
            if required {
                fields.invalid("events", "Required.");
            }
            return None;
        }
        Some(value) => value,
    };
    let Value::Array(items) = value else {
        fields.invalid("events", "Must be a list of event types.");
        return None;
    };
    if items.is_empty() {
        fields.invalid("events", "Choose at least one event.");
        return None;
    }
    let mut events = Vec::new();
    for item in items {
        match item.as_str().and_then(WebhookEventType::parse) {
            Some(kind) => {
                if !events.contains(&kind) {
                    events.push(kind);
                }
            }
            None => {
                fields.invalid("events", "Choose event types an endpoint can subscribe to.");
                return None;
            }
        }
    }
    Some(events)
}

async fn list_webhooks(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let endpoints = state
        .webhooks
        .list_endpoints(access.organization_id, access.environment())
        .await
        .map_err(unavailable)?;
    Ok(envelope(StatusCode::OK, endpoints))
}

/// The contract's `CreatedWebhookEndpoint`.
#[derive(Serialize)]
struct CreatedWebhookEndpoint<'a> {
    endpoint: WebhookEndpoint,
    signing_secret: &'a str,
}

async fn create_webhook(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "add webhook endpoints")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let environment = body
        .string("environment", &mut fields)
        .and_then(|value| match Environment::parse(&value) {
            Some(environment) => Some(environment),
            None => {
                fields.invalid("environment", "Choose sandbox or production.");
                None
            }
        });
    let url = checked_url(body.string("url", &mut fields), &mut fields);
    let description = checked_description(body.string("description", &mut fields), &mut fields);
    let events = checked_events(&body, &mut fields, true);
    fields.finish()?;
    let (Some(environment), Some(url), Some(description), Some(events)) = (environment, url, description, events)
    else {
        return Err(ApiError::internal().into());
    };
    check_public_https(&url, "url")?;
    // A key adds endpoints to its own environment only.
    if access.environment().is_some_and(|own| own != environment) {
        return Err(environment_mismatch());
    }

    let organization_id = access.organization_id;
    let endpoint_id = WebhookEndpointId::generate();
    let Some(secret) = new_signing_secret(environment) else {
        return Err(unavailable(WebhookError::Unavailable));
    };
    let context = EncryptionContext::webhook_signing_secret(organization_id, environment, endpoint_id, SECRET_VERSION);
    let sealed = seal(state.keys.as_ref(), &context, secret.as_bytes())
        .await
        .map_err(|error| {
            tracing::error!(event = "webhooks.seal_failed", %organization_id, %error);
            unavailable(WebhookError::Unavailable)
        })?;
    let endpoint = state
        .webhooks
        .create_endpoint(
            organization_id,
            endpoint_id,
            &NewEndpoint {
                environment,
                url,
                description,
                events,
                secret: sealed,
                secret_last4: secret[secret.len() - 4..].to_owned(),
            },
            Timestamp::now(),
        )
        .await
        .map_err(unavailable)?;
    Ok(envelope(
        StatusCode::CREATED,
        CreatedWebhookEndpoint {
            endpoint,
            signing_secret: &secret,
        },
    ))
}

async fn update_webhook(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, hook)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "edit webhook endpoints")?;
    let endpoint = find(&state, &access, &hook).await?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let url = checked_url(body.optional_string("url", &mut fields), &mut fields);
    let description = checked_description(body.optional_string("description", &mut fields), &mut fields);
    let events = checked_events(&body, &mut fields, false);
    let enabled = match body.0.get("enabled") {
        None => None,
        Some(Value::Bool(enabled)) => Some(*enabled),
        Some(_) => {
            fields.invalid("enabled", "Must be true or false.");
            None
        }
    };
    fields.finish()?;
    if let Some(url) = &url {
        check_public_https(url, "url")?;
    }
    let updated = state
        .webhooks
        .update_endpoint(
            access.organization_id,
            endpoint.id,
            &EndpointChanges {
                url,
                description,
                events,
                enabled,
            },
        )
        .await
        .map_err(|error| match error {
            WebhookError::NotFound => not_found(),
            error => unavailable(error),
        })?;
    Ok(envelope(StatusCode::OK, updated))
}

async fn delete_webhook(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, hook)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "delete webhook endpoints")?;
    let endpoint = find(&state, &access, &hook).await?;
    state
        .webhooks
        .delete_endpoint(access.organization_id, endpoint.id)
        .await
        .map_err(|error| match error {
            WebhookError::NotFound => not_found(),
            error => unavailable(error),
        })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Sends the endpoint a signed `webhook.test` event now, outside the queue,
/// and answers what came back: its status, or `0` when nothing did.
async fn test_webhook(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, hook)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "send test events")?;
    let endpoint = find(&state, &access, &hook).await?;
    let organization_id = access.organization_id;
    let event_id = WebhookEventId::generate();
    let created_at = Timestamp::now();
    let body = event_body(
        event_id,
        TEST_EVENT,
        created_at,
        endpoint.environment,
        organization_id,
        None,
        json!({ "object": "webhook_endpoint", "id": endpoint.id }),
    );
    let sent = webhook_delivery::send(
        &state,
        organization_id,
        endpoint.id,
        &endpoint.url,
        event_id,
        TEST_EVENT,
        &body,
        1,
    )
    .await
    .map_err(|unsent| match unsent {
        // Deleted while it was being tested.
        Unsent::EndpointGone => not_found(),
        // Our failure, not the endpoint's: nothing was sent, nothing is recorded.
        Unsent::StoreUnavailable | Unsent::SecretUnavailable => unavailable(WebhookError::Unavailable),
    })?;
    let at = Timestamp::now();
    let attempt = Attempt {
        number: 1,
        status: sent.status,
        error: sent.error.map(str::to_owned),
        at,
        state: if sent.succeeded() {
            DeliveryState::Succeeded
        } else {
            DeliveryState::Failed
        },
        next_attempt_at: None,
    };
    state
        .webhooks
        .record_test(
            organization_id,
            endpoint.id,
            &NewWebhookEvent {
                id: event_id,
                environment: endpoint.environment,
                workspace_id: None,
                kind: TEST_EVENT.to_owned(),
                body,
                created_at,
            },
            &attempt,
        )
        .await
        .map_err(|error| match error {
            WebhookError::NotFound => not_found(),
            error => unavailable(error),
        })?;
    Ok(envelope(
        StatusCode::OK,
        LastDelivery {
            status_code: sent.status.unwrap_or(0),
            delivered_at: at,
        },
    ))
}
