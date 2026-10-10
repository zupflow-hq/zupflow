//! The audit log, at `/v1/organizations/{org}/audit-events`: Console-only,
//! for owners and admins. Every store writes its events with the change
//! it records; this only reads them, newest first, paged on the server.

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};
use zupflow_core::audit::{AuditError, AuditFilter, DEFAULT_PAGE, MAX_PAGE};
use zupflow_core::error::ApiError;
use zupflow_core::ids::WorkspaceId;
use zupflow_core::organizations::MemberRole;

use crate::auth::{AppState, ConsoleUser, OrganizationAccess};
use crate::http::{Failure, envelope_with_meta};
use crate::runs::query;
use crate::workspaces::{clamped_int, invalid_cursor};

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/organizations/{org}/audit-events", get(list_audit_events))
}

fn audit_failure(error: AuditError) -> Failure {
    match error {
        AuditError::InvalidCursor => invalid_cursor(),
        // The store has logged the cause.
        AuditError::Unavailable => ApiError::new(
            503,
            "unavailable",
            "The service is unavailable right now. Try again shortly.",
        )
        .into(),
    }
}

fn page(items: Value, next_cursor: Option<String>, total: u64) -> Response {
    envelope_with_meta(
        StatusCode::OK,
        items,
        json!({ "pagination": { "next_cursor": next_cursor, "total": total } }),
    )
}

async fn list_audit_events(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "read the audit log")?;
    let query = query(raw.as_deref())?;
    // A malformed workspace ID matches nothing, like one that isn't there.
    let workspace_id = match query.get("workspace_id").filter(|value| !value.is_empty()) {
        None => None,
        Some(value) => match WorkspaceId::parse(value) {
            Ok(workspace_id) => Some(workspace_id),
            Err(_) => return Ok(page(json!([]), None, 0)),
        },
    };
    let limit = clamped_int(
        query.get("limit").map(String::as_str),
        i64::from(DEFAULT_PAGE),
        i64::from(MAX_PAGE),
    );
    let cursor = query
        .get("cursor")
        .map(String::as_str)
        .filter(|cursor| !cursor.is_empty());
    let events = state
        .audit
        .list_audit_events(
            access.organization.id,
            &AuditFilter { workspace_id },
            cursor,
            u32::try_from(limit).unwrap_or(DEFAULT_PAGE),
        )
        .await
        .map_err(audit_failure)?;
    let items = serde_json::to_value(&events.items).map_err(|_| Failure(ApiError::internal()))?;
    Ok(page(items, events.next_cursor, events.total))
}
