//! The organization's tool policy and its levels per catalog app (decision
//! 0040), at `/v1/organizations/{org}/tool-policy` and
//! `/v1/organizations/{org}/apps/{app}/tool-levels`.
//!
//! Every member and the organization's API keys read them; only owners and
//! admins change them, from the Console. They govern both environments, so
//! the Console changes them in Production's name too, as it does an app's
//! auth template. Changing a level rewrites no grant: a call the level now
//! refuses is refused when it's made.

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde::Serialize;
use serde_json::{Value, json};
use zupflow_core::error::ApiError;
use zupflow_core::ids::{AppId, OrganizationId};
use zupflow_core::organizations::{MemberRole, User};
use zupflow_core::policy::{
    AppToolLevel, ApprovalMode, MAX_LEVELS_PER_CHANGE, MAX_TOOL_NAME, NewToolLevel, PolicyError, PolicySettings,
    ToolLevel, ToolPolicy,
};
use zupflow_core::time::Timestamp;

use crate::apps::app_id;
use crate::auth::{Access, AppState, Caller, ConsoleUser, OrganizationAccess};
use crate::http::{Failure, envelope, envelope_with_meta};
use crate::input::{Body, Fields, Object};
use crate::installations::{tool_after, tool_cursor};
use crate::workspaces::{ListQuery, page_limit};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/organizations/{org}/tool-policy",
            get(get_tool_policy).put(update_tool_policy),
        )
        .route(
            "/v1/organizations/{org}/apps/{app}/tool-levels",
            get(list_app_tool_levels).patch(update_app_tool_levels),
        )
}

/// The contract's `ToolPolicy`.
#[derive(Serialize)]
struct ToolPolicyView {
    organization_id: OrganizationId,
    mode: ApprovalMode,
    new_tool_level: NewToolLevel,
    updated_at: Option<Timestamp>,
    updated_by: Option<User>,
}

fn policy_view(policy: ToolPolicy) -> ToolPolicyView {
    ToolPolicyView {
        organization_id: policy.organization_id,
        mode: policy.settings.mode,
        new_tool_level: policy.settings.new_tool_level,
        updated_at: policy.updated_at,
        updated_by: policy.updated_by,
    }
}

/// The contract's `AppToolLevel`.
#[derive(Serialize)]
struct AppToolLevelView {
    app_id: AppId,
    tool: String,
    level: ToolLevel,
    confirmed: bool,
    destructive: bool,
    updated_at: Timestamp,
    updated_by: Option<User>,
}

fn level_view(level: AppToolLevel) -> AppToolLevelView {
    AppToolLevelView {
        app_id: level.app_id,
        tool: level.tool,
        level: level.level,
        confirmed: level.confirmed,
        destructive: level.destructive,
        updated_at: level.updated_at,
        updated_by: level.updated_by,
    }
}

fn policy_failure(error: PolicyError) -> Failure {
    match error {
        PolicyError::AppNotFound => ApiError::not_found("App").into(),
        // The route checks the levels first, field by field.
        PolicyError::InvalidLevels => ApiError::internal().into(),
        // The store has logged the cause.
        PolicyError::Unavailable => ApiError::new(
            503,
            "unavailable",
            "The service is unavailable right now. Try again shortly.",
        )
        .into(),
    }
}

async fn get_tool_policy(
    State(state): State<AppState>,
    caller: Caller,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let policy = state
        .policies
        .load_policy(access.organization_id)
        .await
        .map_err(policy_failure)?;
    Ok(envelope(StatusCode::OK, policy_view(policy)))
}

async fn update_tool_policy(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "change the tool policy")?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let mode = body.string("mode", &mut fields).and_then(|value| {
        let mode = ApprovalMode::parse(&value);
        if mode.is_none() {
            fields.invalid("mode", "Choose manual.");
        }
        mode
    });
    let new_tool_level = body.string("new_tool_level", &mut fields).and_then(|value| {
        let level = NewToolLevel::parse(&value);
        if level.is_none() {
            fields.invalid("new_tool_level", "Choose approve or deny.");
        }
        level
    });
    fields.finish()?;
    let (Some(mode), Some(new_tool_level)) = (mode, new_tool_level) else {
        return Err(ApiError::internal().into());
    };
    let policy = state
        .policies
        .save_policy(
            access.organization.id,
            PolicySettings { mode, new_tool_level },
            Some(access.user.id),
            Timestamp::now(),
        )
        .await
        .map_err(policy_failure)?;
    tracing::info!(
        event = "tool_policy.updated",
        organization_id = %access.organization.id,
        user_id = %access.user.id,
        new_tool_level = new_tool_level.as_str(),
    );
    Ok(envelope(StatusCode::OK, policy_view(policy)))
}

/// A page of a catalog app's levels, by tool name. A workspace app has none: `404`.
async fn list_app_tool_levels(
    State(state): State<AppState>,
    caller: Caller,
    Path((org, app)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Failure> {
    let access = Access::organization(&state, caller, &org).await?;
    let app = app_id(&app)?;
    let query = ListQuery::parse(raw.as_deref())?;
    let after = query
        .cursor
        .as_deref()
        .filter(|cursor| !cursor.is_empty())
        .map(tool_after)
        .transpose()?;
    let page = state
        .policies
        .list_levels(access.organization_id, app, after.as_deref(), page_limit(&query))
        .await
        .map_err(policy_failure)?;
    let next_cursor = page.next_after.as_deref().map(tool_cursor);
    let views: Vec<AppToolLevelView> = page.items.into_iter().map(level_view).collect();
    let meta = json!({ "pagination": { "next_cursor": next_cursor, "total": page.total } });
    Ok(envelope_with_meta(StatusCode::OK, views, meta))
}

/// `UpdateAppToolLevelsInput`, checked: 1 to 100 levels, each tool named
/// once, in 1 to 128 characters, each level allow, approve or deny. Every
/// problem is reported under `levels`.
fn levels_input(body: &Object, fields: &mut Fields) -> Option<Vec<(String, ToolLevel)>> {
    let Some(value) = body.0.get("levels") else {
        fields.invalid("levels", "Required.");
        return None;
    };
    let Value::Array(items) = value else {
        fields.invalid("levels", "Must be a list of levels.");
        return None;
    };
    if items.is_empty() {
        fields.invalid("levels", "Set at least one level.");
        return None;
    }
    if items.len() > MAX_LEVELS_PER_CHANGE {
        fields.invalid("levels", "Set 100 levels or fewer at a time.");
        return None;
    }
    let mut levels: Vec<(String, ToolLevel)> = Vec::with_capacity(items.len());
    for item in items {
        let tool = item.get("tool").and_then(Value::as_str);
        let level = item.get("level").and_then(Value::as_str);
        let problem = match (tool, level) {
            (None, _) | (Some(""), _) => Some("Name the tool."),
            (Some(tool), _) if tool.chars().count() > MAX_TOOL_NAME => Some("Use 128 characters or fewer."),
            (_, level) if level.and_then(ToolLevel::parse).is_none() => Some("Choose allow, approve or deny."),
            (Some(tool), _) if levels.iter().any(|(seen, _)| seen == tool) => Some("Name each tool once."),
            _ => None,
        };
        if let Some(problem) = problem {
            fields.invalid("levels", problem);
            return None;
        }
        if let (Some(tool), Some(level)) = (tool, level.and_then(ToolLevel::parse)) {
            levels.push((tool.to_owned(), level));
        }
    }
    Some(levels)
}

async fn update_app_tool_levels(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path((org, app)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "set tool levels")?;
    let app = app_id(&app)?;
    // The app before the body: a workspace app's levels don't exist to set.
    state
        .policies
        .list_levels(access.organization.id, app, None, 1)
        .await
        .map_err(policy_failure)?;
    let body = body.object()?;
    let mut fields = Fields::default();
    let levels = levels_input(&body, &mut fields);
    fields.finish()?;
    let Some(levels) = levels else {
        return Err(ApiError::internal().into());
    };
    let written = state
        .policies
        .set_levels(
            access.organization.id,
            app,
            &levels,
            Some(access.user.id),
            Timestamp::now(),
        )
        .await
        .map_err(policy_failure)?;
    tracing::info!(
        event = "tool_policy.levels_set",
        organization_id = %access.organization.id,
        %app,
        user_id = %access.user.id,
        levels = written.len(),
    );
    Ok(envelope(
        StatusCode::OK,
        written.into_iter().map(level_view).collect::<Vec<_>>(),
    ))
}
