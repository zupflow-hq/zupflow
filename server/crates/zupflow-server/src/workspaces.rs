//! Workspaces: listed and created under their organization, then reached at
//! `/v1/workspaces/{ws}` with the organization named by the request.

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use serde_json::json;
use zupflow_core::audit::{AuditAction, AuditEntry, AuditObject, AuditPlace, AuditTarget};
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
pub(crate) struct ListQuery {
    pub(crate) environment: Option<String>,
    pub(crate) q: Option<String>,
    pub(crate) sort: Option<String>,
    pub(crate) cursor: Option<String>,
    pub(crate) limit: Option<String>,
}

impl ListQuery {
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self, Failure> {
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

/// A list's `environment` filter. A key sees its own environment only:
/// asking for the other is `422`, asking for none means its own.
pub(crate) fn listed_environment(access: &Access, query: &ListQuery) -> Result<Option<Environment>, Failure> {
    let asked = match query.environment.as_deref().filter(|value| !value.is_empty()) {
        None => None,
        Some(value) => Some(Environment::parse(value).ok_or_else(|| {
            let mut error = ApiError::new(400, "validation_failed", "Unknown environment.");
            error.fields = Some([("environment".to_owned(), "Unknown.".to_owned())].into());
            Failure(error)
        })?),
    };
    match (access.environment(), asked) {
        (Some(own), Some(asked)) if own != asked => Err(environment_mismatch()),
        (Some(own), _) => Ok(Some(own)),
        (None, asked) => Ok(asked),
    }
}

/// A list's page size: 1 to 100, default 25; out of range is clamped, not an
/// integer is the default.
pub(crate) fn page_limit(query: &ListQuery) -> u32 {
    u32::try_from(clamped_int(query.limit.as_deref(), 25, 100)).unwrap_or(25)
}

/// A whole-number query parameter read as the mock's `clampedInt` reads it,
/// with JavaScript's `Number`: missing is `fallback`; empty is 0; `5.0` and
/// `1e2` are integers and `0x10` is 16; anything that isn't an integer
/// (`1.5`, `abc`, `Infinity`) is `fallback`. An integer is clamped to `1..=max`.
pub(crate) fn clamped_int(value: Option<&str>, fallback: i64, max: i64) -> i64 {
    let Some(value) = value else { return fallback };
    let value = value.trim();
    let number = if value.is_empty() {
        Some(0.0)
    } else if let Some((radix, digits)) = [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)]
        .into_iter()
        .find_map(|(prefix, radix)| value.strip_prefix(prefix).map(|digits| (radix, digits)))
    {
        if digits.is_empty() || !digits.chars().all(|digit| digit.is_digit(radix)) {
            None
        } else {
            // Exact enough: anything past `max` is clamped.
            Some(
                digits
                    .chars()
                    .filter_map(|digit| digit.to_digit(radix))
                    .fold(0.0, |total, digit| total * f64::from(radix) + f64::from(digit)),
            )
        }
    } else if value
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
    {
        // JavaScript's decimal literal; Rust's parser also takes `inf` and
        // `nan`, which this leaves out, as `Number` does.
        value.parse::<f64>().ok()
    } else {
        None
    };
    match number {
        #[allow(clippy::cast_possible_truncation)]
        Some(number) if number.is_finite() && number.fract() == 0.0 => number.clamp(1.0, max as f64) as i64,
        _ => fallback,
    }
}

/// A key asked for its other environment.
pub(crate) fn environment_mismatch() -> Failure {
    ApiError::new(
        422,
        "environment_mismatch",
        "This API key works in one environment only; use a key for the other one.",
    )
    .into()
}

pub(crate) fn invalid_cursor() -> Failure {
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
    let environment = listed_environment(&access, &query)?;
    let limit = page_limit(&query);
    let filter = WorkspaceFilter {
        environment,
        query: query.q,
        sort: if query.sort.as_deref() == Some("activity") {
            WorkspaceSort::Activity
        } else {
            WorkspaceSort::Name
        },
    };
    let page = state
        .workspaces
        .list_workspaces(
            access.organization_id,
            &filter,
            query.cursor.as_deref().filter(|cursor| !cursor.is_empty()),
            limit,
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
            &|workspace: &Workspace| {
                AuditEntry::new(
                    access.audit_actor(),
                    AuditAction::WorkspaceCreated,
                    AuditTarget::new(AuditObject::Workspace(workspace.id), &workspace.name),
                    AuditPlace::Workspace(workspace.id),
                )
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
            &|workspace: &Workspace| {
                AuditEntry::new(
                    access.access.audit_actor(),
                    AuditAction::WorkspaceUpdated,
                    AuditTarget::new(AuditObject::Workspace(workspace.id), &workspace.name),
                    AuditPlace::Workspace(workspace.id),
                )
            },
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
        // Its history stays; the deletion is recorded in its environment.
        .delete_workspace(
            access.access.organization_id,
            access.workspace.id,
            &|workspace: &Workspace| {
                AuditEntry::new(
                    access.access.audit_actor(),
                    AuditAction::WorkspaceDeleted,
                    AuditTarget::new(AuditObject::Workspace(workspace.id), &workspace.name),
                    // Recorded before it goes, keeping its ID: its history finds it.
                    AuditPlace::Workspace(workspace.id),
                )
            },
        )
        .await
        .map_err(|error| match error {
            WorkspaceError::NotFound => ApiError::not_found("Workspace").into(),
            error => workspace_unavailable(error),
        })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::clamped_int;

    #[test]
    fn reads_whole_numbers_as_javascripts_number_does() {
        for (value, read) in [
            (None, 14),
            (Some(""), 1),
            (Some(" 7 "), 7),
            (Some("5.0"), 5),
            (Some("1e1"), 10),
            (Some("1e20"), 90),
            (Some("0x10"), 16),
            (Some("0b11"), 3),
            (Some("0"), 1),
            (Some("-3"), 1),
            (Some("1.5"), 14),
            (Some("seven"), 14),
            (Some("Infinity"), 14),
            (Some("inf"), 14),
            (Some("NaN"), 14),
            (Some("0x"), 14),
            (Some("-0x10"), 14),
            (Some("1e"), 14),
        ] {
            assert_eq!(clamped_int(value, 14, 90), read, "{value:?}");
        }
    }
}
