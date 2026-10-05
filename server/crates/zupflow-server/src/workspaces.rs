//! Workspaces: listed and created under their organization, then reached at
//! `/v1/workspaces/{ws}` with the organization named by the request.

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use serde_json::json;
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::organizations::MemberRole;
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::{
    NewWorkspace, Workspace, WorkspaceError, WorkspaceFilter, WorkspaceSort, WorkspaceStats,
};

use crate::auth::{Access, AppState, Caller, WorkspaceAccess, workspace_unavailable};
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Fields, Object};
use crate::organizations::{checked_name, checked_slug};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/organizations/{org}/workspaces",
            get(list_workspaces).post(create_workspace),
        )
        .route(
            "/v1/workspaces/{ws}",
            get(get_workspace).patch(update_workspace).delete(delete_workspace),
        )
}

const EXTERNAL_ID_MAX: usize = 120;

/// The contract's `Workspace`.
#[derive(Serialize)]
struct WorkspaceView {
    id: WorkspaceId,
    organization_id: OrganizationId,
    name: String,
    slug: String,
    environment: Environment,
    external_id: Option<String>,
    created_at: Timestamp,
    stats: WorkspaceStats,
}

/// The workspace with its stats, read in `organization_id`: the organization
/// the trust context established, never the row's.
async fn view(
    state: &AppState,
    organization_id: OrganizationId,
    workspace: Workspace,
) -> Result<WorkspaceView, Failure> {
    let stats = state
        .workspaces
        .load_stats(organization_id, workspace.id)
        .await
        .map_err(workspace_unavailable)?;
    Ok(WorkspaceView {
        id: workspace.id,
        organization_id: workspace.organization_id,
        name: workspace.name,
        slug: workspace.slug,
        environment: workspace.environment,
        external_id: workspace.external_id,
        created_at: workspace.created_at,
        stats,
    })
}

/// An External ID: a string, trimmed, at most 120 characters, or `null`. A
/// blank one is `null`.
fn checked_external_id(body: &Object, fields: &mut Fields) -> Option<Option<String>> {
    match body.0.get("external_id")? {
        serde_json::Value::Null => Some(None),
        serde_json::Value::String(value) => {
            let value = value.trim();
            if value.chars().count() > EXTERNAL_ID_MAX {
                fields.invalid("external_id", "Use 120 characters or fewer.");
                None
            } else {
                Some((!value.is_empty()).then(|| value.to_owned()))
            }
        }
        _ => {
            fields.invalid("external_id", "Must be a string or null.");
            None
        }
    }
}

/// The list's query parameters, read after the caller's access is
/// established and never rejected by the framework: a repeated parameter
/// takes its first value, as the mock does, and an unreadable query string
/// answers the envelope's `400`.
#[derive(Default)]
struct ListQuery {
    environment: Option<String>,
    q: Option<String>,
    sort: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

impl ListQuery {
    fn parse(raw: Option<&str>) -> Result<Self, Failure> {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_str(raw.unwrap_or_default()).map_err(|_| {
            Failure(ApiError::new(
                400,
                "validation_failed",
                "The query string can't be read.",
            ))
        })?;
        let mut query = Self::default();
        for (name, value) in pairs {
            let slot = match name.as_str() {
                "environment" => &mut query.environment,
                "q" => &mut query.q,
                "sort" => &mut query.sort,
                "cursor" => &mut query.cursor,
                "limit" => &mut query.limit,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(value);
            }
        }
        Ok(query)
    }
}

/// A key asked for its other environment.
fn environment_mismatch() -> Failure {
    ApiError::new(
        422,
        "environment_mismatch",
        "This API key works in one environment only; use a key for the other one.",
    )
    .into()
}

fn invalid_cursor() -> Failure {
    let mut error = ApiError::new(400, "validation_failed", "Some fields are invalid.");
    error.fields = Some([("cursor".to_owned(), "Not a cursor from this list.".to_owned())].into());
    error.into()
}

async fn list_workspaces(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let query = ListQuery::parse(raw.as_deref())?;
    let environment = match query.environment.as_deref().filter(|value| !value.is_empty()) {
        None => None,
        Some(value) => Some(Environment::parse(value).ok_or_else(|| {
            let mut error = ApiError::new(400, "validation_failed", "Unknown environment.");
            error.fields = Some([("environment".to_owned(), "Unknown.".to_owned())].into());
            Failure(error)
        })?),
    };
    // A key sees its own environment only.
    let environment = match (access.environment(), environment) {
        (Some(own), Some(asked)) if own != asked => return Err(environment_mismatch()),
        (Some(own), _) => Some(own),
        (None, asked) => asked,
    };
    let filter = WorkspaceFilter {
        environment,
        query: query.q,
        sort: if query.sort.as_deref() == Some("activity") {
            WorkspaceSort::Activity
        } else {
            WorkspaceSort::Name
        },
    };
    // 1 to 100, default 25; out of range is clamped, not an integer is the default.
    let limit = query
        .limit
        .as_deref()
        .and_then(|value| {
            if value.trim().is_empty() {
                Some(0)
            } else {
                value.trim().parse::<i64>().ok()
            }
        })
        .map_or(25, |limit| limit.clamp(1, 100));
    let page = state
        .workspaces
        .list_workspaces(
            access.organization_id,
            &filter,
            query.cursor.as_deref().filter(|cursor| !cursor.is_empty()),
            u32::try_from(limit).unwrap_or(25),
        )
        .await
        .map_err(|error| match error {
            WorkspaceError::InvalidCursor => invalid_cursor(),
            error => workspace_unavailable(error),
        })?;
    let mut items = Vec::with_capacity(page.items.len());
    for workspace in page.items {
        items.push(view(&state, access.organization_id, workspace).await?);
    }
    let meta = json!({ "pagination": { "next_cursor": page.next_cursor, "total": page.total } });
    Ok(envelope_with_meta(StatusCode::OK, items, meta))
}

async fn create_workspace(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    access.require(MemberRole::is_admin, "create workspaces")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let name = checked_name(body.string("name", &mut fields), &mut fields);
    let slug = checked_slug(body.string("slug", &mut fields), &mut fields);
    let environment = body
        .string("environment", &mut fields)
        .and_then(|value| match Environment::parse(&value) {
            Some(environment) => Some(environment),
            None => {
                fields.invalid("environment", "Choose sandbox or production.");
                None
            }
        });
    // Required, though it may be null.
    if !body.has("external_id") {
        fields.invalid("external_id", "Required; send null for none.");
    }
    let external_id = checked_external_id(&body, &mut fields);
    fields.finish()?;
    let (Some(name), Some(slug), Some(environment), Some(external_id)) = (name, slug, environment, external_id) else {
        return Err(ApiError::internal().into());
    };
    if access.environment().is_some_and(|own| own != environment) {
        return Err(environment_mismatch());
    }
    let workspace = state
        .workspaces
        .create_workspace(
            access.organization_id,
            WorkspaceId::generate(),
            &NewWorkspace {
                name,
                slug,
                environment,
                external_id,
            },
        )
        .await
        .map_err(|error| match error {
            WorkspaceError::SlugTaken => {
                let mut error = ApiError::new(409, "slug_taken", "Another workspace uses that slug.");
                error.fields = Some([("slug".to_owned(), "Already used in this organization.".to_owned())].into());
                Failure(error)
            }
            error => workspace_unavailable(error),
        })?;
    Ok(envelope(
        StatusCode::CREATED,
        view(&state, access.organization_id, workspace).await?,
    ))
}

async fn get_workspace(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    let organization_id = access.access.organization_id;
    Ok(envelope(
        StatusCode::OK,
        view(&state, organization_id, access.workspace).await?,
    ))
}

async fn update_workspace(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(MemberRole::is_admin, "change workspace settings")?;
    let body = body.object()?;
    if body.has("environment") {
        return Err(ApiError::new(
            422,
            "environment_immutable",
            "A workspace's environment can't change. Create a workspace in the other environment instead.",
        )
        .into());
    }
    let mut fields = Fields::default();
    let name = checked_name(body.optional_string("name", &mut fields), &mut fields);
    let external_id = checked_external_id(&body, &mut fields);
    fields.finish()?;
    let workspace = state
        .workspaces
        .update_workspace(
            access.access.organization_id,
            access.workspace.id,
            name.as_deref(),
            external_id.as_ref().map(|external_id| external_id.as_deref()),
        )
        .await
        .map_err(|error| match error {
            WorkspaceError::NotFound => ApiError::not_found("Workspace").into(),
            error => workspace_unavailable(error),
        })?;
    Ok(envelope(
        StatusCode::OK,
        view(&state, access.access.organization_id, workspace).await?,
    ))
}

async fn delete_workspace(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(ws): Path<String>,
) -> Result<Response, Failure> {
    let access = WorkspaceAccess::resolve(&state, caller, &headers, &ws).await?;
    access
        .access
        .require(|role| role == MemberRole::Owner, "delete workspaces")?;
    state
        .workspaces
        .delete_workspace(access.access.organization_id, access.workspace.id)
        .await
        .map_err(|error| match error {
            WorkspaceError::NotFound => ApiError::not_found("Workspace").into(),
            error => workspace_unavailable(error),
        })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
