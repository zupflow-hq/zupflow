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
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use serde::Serialize;
use serde_json::{Value, json};
use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{WebhookDeliveryId, WebhookEndpointId, WebhookEventId, WorkspaceId};
use zupflow_core::keys::{EncryptionContext, seal};
use zupflow_core::organizations::MemberRole;
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::{
    Attempt, DEFAULT_DELIVERY_PAGE, DeliveryFilter, DeliveryState, EndpointChanges, LastDelivery, MAX_DELIVERY_PAGE,
    NewEndpoint, NewWebhookEvent, SECRET_VERSION, TEST_EVENT, WebhookEndpoint, WebhookError, WebhookEventType,
    event_body, new_signing_secret,
};

use crate::app_configurations::check_public_https;
use crate::auth::{Access, AppState, Caller};
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Fields, Object};
use crate::runs::query;
use crate::webhook_delivery::{self, Unsent};
use crate::workspaces::{clamped_int, environment_mismatch, invalid_cursor};

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
        .route(
            "/v1/organizations/{org}/webhooks/{hook}/deliveries",
            get(list_deliveries),
        )
        .route(
            "/v1/organizations/{org}/webhooks/{hook}/deliveries/{delivery}",
            get(get_delivery),
        )
        .route(
            "/v1/organizations/{org}/webhooks/{hook}/deliveries/{delivery}/attempts",
            get(list_attempts),
        )
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

fn history_error(error: WebhookError) -> Failure {
    match error {
        WebhookError::NotFound => ApiError::not_found("Webhook delivery").into(),
        WebhookError::InvalidCursor => invalid_cursor(),
        WebhookError::Unavailable => unavailable(error),
    }
}

fn history_page<T: Serialize>(page: zupflow_core::workspaces::Page<T>) -> Response {
    envelope_with_meta(
        StatusCode::OK,
        page.items,
        json!({ "pagination": { "next_cursor": page.next_cursor, "total": page.total } }),
    )
}

fn delivery_id(value: &str) -> Result<WebhookDeliveryId, Failure> {
    WebhookDeliveryId::parse(value).map_err(|_| ApiError::not_found("Webhook delivery").into())
}

async fn list_deliveries(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, hook)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let endpoint = find(&state, &access, &hook).await?;
    let query = query(raw.as_deref())?;
    let mut filter = DeliveryFilter::default();
    if let Some(workspace) = query.get("workspace_id") {
        filter.workspace_id = WorkspaceId::parse(workspace).ok();
        filter.matches_nothing |= filter.workspace_id.is_none();
    }
    if let Some(value) = query.get("state") {
        filter.state = DeliveryState::parse(value);
        filter.matches_nothing |= filter.state.is_none();
    }
    filter.event_type = query.get("event_type").cloned();
    if let Some(kind) = &filter.event_type {
        filter.matches_nothing |= kind != TEST_EVENT && WebhookEventType::parse(kind).is_none();
    }
    let limit = clamped_int(
        query.get("limit").map(String::as_str),
        i64::from(DEFAULT_DELIVERY_PAGE),
        i64::from(MAX_DELIVERY_PAGE),
    );
    let page = state
        .webhooks
        .list_deliveries(
            access.organization_id,
            endpoint.id,
            endpoint.environment,
            &filter,
            query.get("cursor").map(String::as_str),
            u32::try_from(limit).unwrap_or(DEFAULT_DELIVERY_PAGE),
        )
        .await
        .map_err(history_error)?;
    Ok(history_page(page))
}

async fn get_delivery(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, hook, delivery)): Path<(String, String, String)>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let endpoint = find(&state, &access, &hook).await?;
    let detail = state
        .webhooks
        .find_delivery_detail(
            access.organization_id,
            endpoint.id,
            endpoint.environment,
            delivery_id(&delivery)?,
        )
        .await
        .map_err(history_error)?
        .ok_or_else(|| history_error(WebhookError::NotFound))?;
    Ok(envelope(StatusCode::OK, detail))
}

async fn list_attempts(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, hook, delivery)): Path<(String, String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let endpoint = find(&state, &access, &hook).await?;
    let id = delivery_id(&delivery)?;
    // Find the child before reading query parameters, as every resource read
    // does. The paginated store repeats this inside its own snapshot.
    state
        .webhooks
        .find_delivery_detail(access.organization_id, endpoint.id, endpoint.environment, id)
        .await
        .map_err(history_error)?
        .ok_or_else(|| history_error(WebhookError::NotFound))?;
    let query = query(raw.as_deref())?;
    let limit = clamped_int(
        query.get("limit").map(String::as_str),
        i64::from(DEFAULT_DELIVERY_PAGE),
        i64::from(MAX_DELIVERY_PAGE),
    );
    let page = state
        .webhooks
        .list_delivery_attempts(
            access.organization_id,
            endpoint.id,
            endpoint.environment,
            id,
            query.get("cursor").map(String::as_str),
            u32::try_from(limit).unwrap_or(DEFAULT_DELIVERY_PAGE),
        )
        .await
        .map_err(history_error)?;
    Ok(history_page(page))
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
            &|endpoint: &WebhookEndpoint| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::WebhookCreated,
                    webhook_target(endpoint),
                    AuditPlace::Environment(endpoint.environment),
                )
            },
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

/// An endpoint as the audit log names it: by its URL's scheme, host and
/// path. A query or fragment may carry a token, so neither is recorded.
fn webhook_target(endpoint: &WebhookEndpoint) -> AuditTarget {
    AuditTarget::new(AuditObject::Webhook(endpoint.id), audit_url(&endpoint.url))
}

/// `url` without its query, fragment or credentials: what the log may keep.
pub(crate) fn audit_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or_default();
            match parsed.port() {
                Some(port) => format!("{}://{host}:{port}{}", parsed.scheme(), parsed.path()),
                None => format!("{}://{host}{}", parsed.scheme(), parsed.path()),
            }
        }
        Err(_) => "Webhook endpoint".to_owned(),
    }
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
            &|endpoint: &WebhookEndpoint| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::WebhookUpdated,
                    webhook_target(endpoint),
                    AuditPlace::Environment(endpoint.environment),
                )
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
        .delete_endpoint(access.organization_id, endpoint.id, &|endpoint: &WebhookEndpoint| {
            AuditEntry::new(
                access.audit_actor(),
                AuditAction::WebhookDeleted,
                webhook_target(endpoint),
                AuditPlace::Environment(endpoint.environment),
            )
        })
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
        duration_ms: sent.duration_ms,
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

#[cfg(test)]
mod tests {
    use super::audit_url;

    #[test]
    fn keeps_only_scheme_host_and_path_for_the_audit_log() {
        for (url, kept) in [
            ("https://hooks.example/in?token=secret#top", "https://hooks.example/in"),
            ("https://HOOKS.example:8443/a/b/?x=1", "https://hooks.example:8443/a/b/"),
            ("https://user:pass@hooks.example/in", "https://hooks.example/in"),
            ("https://hooks.example", "https://hooks.example/"),
            ("not a url", "Webhook endpoint"),
        ] {
            assert_eq!(audit_url(url), kept, "{url}");
        }
    }
}
