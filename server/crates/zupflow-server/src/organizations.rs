//! `/v1/me`, organizations and members: the Console's routes for who you are
//! and who you work with. All of them are Console-only.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use serde::Serialize;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{MemberId, OrganizationId};
use zupflow_core::organizations::{Member, MemberRole, Organization, OrganizationSetup, StoreError, User};
use zupflow_core::time::Timestamp;

use crate::auth::{AppState, ConsoleUser, OrganizationAccess, unavailable};
use crate::http::{Failure, envelope};
use crate::input::{Body, Fields};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/me", get(viewer))
        .route("/v1/organizations", axum::routing::post(create_organization))
        .route(
            "/v1/organizations/{org}",
            get(get_organization).patch(update_organization),
        )
        .route("/v1/organizations/{org}/members", get(list_members).post(invite_member))
        .route(
            "/v1/organizations/{org}/members/{member}",
            patch(update_member).delete(remove_member),
        )
}

/// Paths in the Console's URLs; an organization can't take one as its slug.
const RESERVED_SLUGS: [&str; 8] = [
    "onboarding",
    "sign-in",
    "sign-up",
    "connect",
    "settings",
    "api",
    "new",
    "help",
];

const NAME_MAX: usize = 60;

/// The contract's `Organization`: the organization as the caller sees it.
#[derive(Serialize)]
struct OrganizationView {
    id: OrganizationId,
    name: String,
    slug: String,
    role: MemberRole,
    created_at: Timestamp,
    setup: OrganizationSetup,
}

#[derive(Serialize)]
struct Viewer {
    user: User,
    organizations: Vec<OrganizationView>,
}

async fn view(state: &AppState, organization: Organization, role: MemberRole) -> Result<OrganizationView, Failure> {
    let setup = state
        .organizations
        .load_setup(organization.id)
        .await
        .map_err(unavailable)?;
    Ok(OrganizationView {
        id: organization.id,
        name: organization.name,
        slug: organization.slug,
        role,
        created_at: organization.created_at,
        setup,
    })
}

/// A name: trimmed, 1–60 characters.
fn checked_name(value: Option<String>, fields: &mut Fields) -> Option<String> {
    let name = value?.trim().to_owned();
    if name.is_empty() {
        fields.invalid("name", "Required.");
        None
    } else if name.chars().count() > NAME_MAX {
        fields.invalid("name", "Use 60 characters or fewer.");
        None
    } else {
        Some(name)
    }
}

/// A slug: 1–40 lowercase letters, numbers and hyphens, not starting or ending with a hyphen.
fn checked_slug(value: Option<String>, fields: &mut Fields) -> Option<String> {
    let slug = value?;
    let valid = (1..=40).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-')
        && !slug.ends_with('-');
    if valid {
        Some(slug)
    } else {
        fields.invalid("slug", "Use 1–40 lowercase letters, numbers and hyphens.");
        None
    }
}

fn reserved(slug: &str, field_message: &str) -> Result<(), Failure> {
    if RESERVED_SLUGS.contains(&slug) {
        let mut error = ApiError::new(422, "slug_reserved", "That URL is reserved.");
        error.fields = Some([("slug".to_owned(), field_message.to_owned())].into());
        return Err(error.into());
    }
    Ok(())
}

fn slug_taken() -> Failure {
    let mut error = ApiError::new(409, "slug_taken", "That URL is taken.");
    error.fields = Some([("slug".to_owned(), "Already taken.".to_owned())].into());
    error.into()
}

fn checked_role(value: Option<String>, fields: &mut Fields) -> Option<MemberRole> {
    let role = value?;
    let parsed = MemberRole::parse(&role);
    if parsed.is_none() {
        fields.invalid("role", "Choose owner, admin, developer or viewer.");
    }
    parsed
}

/// An email address, trimmed and lowercased, by the contract's rule (zod's
/// `email()`): dot-separated local parts of letters, digits and `_'+-` (the
/// last not ending in `'`), then dot-separated domain labels that start with
/// a letter or digit, and a final label of two or more letters.
fn checked_email(value: Option<String>, fields: &mut Fields) -> Option<String> {
    let email = value?.trim().to_lowercase();
    if email.len() <= 320 && valid_email(&email) {
        Some(email)
    } else {
        fields.invalid("email", "Enter a valid email address.");
        None
    }
}

fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    let local_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '\'' | '+' | '-');
    let local_ok = local
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(local_char))
        && !local.ends_with('\'');
    let labels: Vec<&str> = domain.split('.').collect();
    let Some((last, rest)) = labels.split_last() else {
        return false;
    };
    let domain_ok = !rest.is_empty()
        && rest.iter().all(|label| {
            label.starts_with(|c: char| c.is_ascii_alphanumeric())
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        && last.len() >= 2
        && last.chars().all(|c| c.is_ascii_alphabetic());
    local_ok && domain_ok
}

fn member_not_found() -> Failure {
    ApiError::not_found("Member").into()
}

async fn viewer(State(state): State<AppState>, ConsoleUser(user): ConsoleUser) -> Result<Response, Failure> {
    let memberships = state
        .organizations
        .list_active_memberships_of_user(user.id)
        .await
        .map_err(unavailable)?;
    let mut organizations = Vec::with_capacity(memberships.len());
    for (organization, member) in memberships {
        organizations.push(view(&state, organization, member.role).await?);
    }
    Ok(envelope(StatusCode::OK, Viewer { user, organizations }))
}

async fn create_organization(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    body: Body,
) -> Result<Response, Failure> {
    let mut fields = Fields::default();
    let name = checked_name(body.string("name", &mut fields), &mut fields);
    let slug = checked_slug(body.string("slug", &mut fields), &mut fields);
    fields.finish()?;
    let (Some(name), Some(slug)) = (name, slug) else {
        return Err(ApiError::internal().into());
    };
    reserved(&slug, "Reserved. Choose another.")?;
    let (organization, member) = state
        .organizations
        .create_organization(OrganizationId::generate(), &name, &slug, user.id)
        .await
        .map_err(|error| match error {
            StoreError::SlugTaken => slug_taken(),
            error => unavailable(error),
        })?;
    Ok(envelope(
        StatusCode::CREATED,
        view(&state, organization, member.role).await?,
    ))
}

async fn get_organization(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    Ok(envelope(
        StatusCode::OK,
        view(&state, access.organization, access.member.role).await?,
    ))
}

async fn update_organization(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "change organization settings")?;
    let mut fields = Fields::default();
    let name = checked_name(body.optional_string("name", &mut fields), &mut fields);
    let slug = checked_slug(body.optional_string("slug", &mut fields), &mut fields);
    fields.finish()?;
    let slug = slug.filter(|slug| *slug != access.organization.slug);
    if let Some(slug) = &slug {
        reserved(slug, "Reserved.")?;
    }
    let organization = state
        .organizations
        .update_organization(access.organization.id, name.as_deref(), slug.as_deref())
        .await
        .map_err(|error| match error {
            StoreError::SlugTaken => slug_taken(),
            error => unavailable(error),
        })?;
    Ok(envelope(
        StatusCode::OK,
        view(&state, organization, access.member.role).await?,
    ))
}

async fn list_members(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    let members: Vec<Member> = state
        .organizations
        .list_members(access.organization.id)
        .await
        .map_err(unavailable)?;
    Ok(envelope(StatusCode::OK, members))
}

async fn invite_member(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path(org): Path<String>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "invite members")?;
    let mut fields = Fields::default();
    let email = checked_email(body.string("email", &mut fields), &mut fields);
    let role = checked_role(body.string("role", &mut fields), &mut fields);
    fields.finish()?;
    let (Some(email), Some(role)) = (email, role) else {
        return Err(ApiError::internal().into());
    };
    if role == MemberRole::Owner && access.member.role != MemberRole::Owner {
        return Err(ApiError::forbidden("Only owners can invite owners.").into());
    }
    let member = state
        .organizations
        .invite_member(access.organization.id, &email, role)
        .await
        .map_err(|error| match error {
            StoreError::AlreadyMember => {
                let mut error = ApiError::new(
                    409,
                    "already_member",
                    format!("{email} is already in this organization."),
                );
                error.fields = Some([("email".to_owned(), "Already a member or invited.".to_owned())].into());
                Failure(error)
            }
            error => unavailable(error),
        })?;
    Ok(envelope(StatusCode::CREATED, member))
}

fn last_owner() -> Failure {
    ApiError::new(422, "last_owner", "An organization needs at least one owner.").into()
}

async fn update_member(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path((org, member)): Path<(String, String)>,
    body: Body,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(|role| role == MemberRole::Owner, "change roles")?;
    let mut fields = Fields::default();
    let role = checked_role(body.string("role", &mut fields), &mut fields);
    fields.finish()?;
    let Some(role) = role else {
        return Err(ApiError::internal().into());
    };
    let member_id = MemberId::parse(&member).map_err(|_| member_not_found())?;
    let member = state
        .organizations
        .update_member_role(access.organization.id, member_id, role, access.member.id)
        .await
        .map_err(|error| match error {
            StoreError::NotFound => member_not_found(),
            // Demoted since the check above: the store reads the role again.
            StoreError::NotAllowed => {
                ApiError::forbidden("Your role can't change roles any more. Ask an organization owner.").into()
            }
            StoreError::LastOwner => last_owner(),
            error => unavailable(error),
        })?;
    Ok(envelope(StatusCode::OK, member))
}

async fn remove_member(
    State(state): State<AppState>,
    ConsoleUser(user): ConsoleUser,
    Path((org, member)): Path<(String, String)>,
) -> Result<Response, Failure> {
    let access = OrganizationAccess::resolve(&state, user, &org).await?;
    access.require(MemberRole::is_admin, "remove members")?;
    let member_id = MemberId::parse(&member).map_err(|_| member_not_found())?;
    // The store checks in one transaction: the caller's role as of now (403),
    // missing (404), an owner removed by a non-owner (403), then the last
    // active owner (422).
    state
        .organizations
        .remove_member(access.organization.id, member_id, access.member.id)
        .await
        .map_err(|error| match error {
            StoreError::NotFound => member_not_found(),
            // An owner removed by a non-owner, or a caller demoted since the check above.
            StoreError::NotAllowed => ApiError::forbidden(format!(
                "Your role ({}) can't remove that member. Ask an organization owner.",
                access.member.role.as_str()
            ))
            .into(),
            StoreError::LastOwner => last_owner(),
            error => unavailable(error),
        })?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
