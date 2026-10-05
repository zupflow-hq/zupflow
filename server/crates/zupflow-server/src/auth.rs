//! The trust context for Console requests: who is calling, then which
//! organization they act in. Both are established before anything else, so a
//! signed-out caller gets `401` before any ID is parsed, and an organization
//! the caller isn't an active member of is `404`, exactly like one that
//! doesn't exist.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use zupflow_core::error::ApiError;
use zupflow_core::identity::{Identity, IdentityError};
use zupflow_core::ids::OrganizationId;
use zupflow_core::ids::WorkspaceId;
use zupflow_core::organizations::{Member, MemberRole, Organization, OrganizationStore, StoreError, User};
use zupflow_core::workspaces::{Workspace, WorkspaceError, WorkspaceStore};

use crate::http::Failure;

/// What every route reaches its adapters through.
#[derive(Clone)]
pub struct AppState {
    pub identity: Arc<dyn Identity>,
    pub organizations: Arc<dyn OrganizationStore>,
    pub workspaces: Arc<dyn WorkspaceStore>,
}

/// A store failure the caller can do nothing about: the database is a
/// required dependency, so `503`. The store has logged the cause.
pub fn unavailable(_: StoreError) -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

/// The signed-in Console user.
pub struct ConsoleUser(pub User);

impl FromRequestParts<AppState> for ConsoleUser {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let identity = state.identity.verify(bearer).await.map_err(|error| match error {
            IdentityError::Unauthenticated => Failure(ApiError::unauthenticated()),
            IdentityError::Unavailable => {
                tracing::error!(event = "auth.identity_unavailable");
                Failure(ApiError::new(
                    503,
                    "identity_unavailable",
                    "Sign-in is unavailable right now. Try again shortly.",
                ))
            }
        })?;
        match state.organizations.user_for_identity(&identity).await {
            Ok(user) => Ok(Self(user)),
            Err(StoreError::EmailUnverified) => {
                tracing::warn!(event = "auth.email_unverified", provider = %identity.provider);
                Err(ApiError::new(
                    401,
                    "email_unverified",
                    "Verify your email address with your sign-in provider, then sign in again.",
                )
                .into())
            }
            Err(StoreError::EmailInUse) => {
                tracing::warn!(event = "auth.email_in_use", provider = %identity.provider);
                Err(ApiError::new(
                    401,
                    "email_in_use",
                    "Another sign-in already uses this email address. Sign in the way you did before.",
                )
                .into())
            }
            Err(error) => Err(unavailable(error)),
        }
    }
}

/// A Console user acting in one organization they're an active member of.
pub struct OrganizationAccess {
    pub user: User,
    pub organization: Organization,
    pub member: Member,
}

impl OrganizationAccess {
    /// Resolves the organization a route names. A malformed ID, a missing
    /// organization, and one the user isn't an active member of (an
    /// invitation included) all answer the same `404`.
    pub async fn resolve(state: &AppState, user: User, organization: &str) -> Result<Self, Failure> {
        Self::resolve_hiding_as(state, user, organization, "Organization").await
    }

    /// Like [`OrganizationAccess::resolve`], with the `404` naming `what`: a
    /// workspace route says the workspace wasn't found, whichever was missing.
    async fn resolve_hiding_as(state: &AppState, user: User, organization: &str, what: &str) -> Result<Self, Failure> {
        let not_found = || Failure(ApiError::not_found(what));
        let organization_id = OrganizationId::parse(organization).map_err(|_| not_found())?;
        let (organization, member) = state
            .organizations
            .find_active_membership(organization_id, user.id)
            .await
            .map_err(unavailable)?
            .ok_or_else(not_found)?;
        Ok(Self {
            user,
            organization,
            member,
        })
    }

    /// `403` unless the caller's role is one of `roles`.
    pub fn require(&self, allowed: impl Fn(MemberRole) -> bool, action: &str) -> Result<(), Failure> {
        if allowed(self.member.role) {
            return Ok(());
        }
        Err(ApiError::forbidden(format!(
            "Your role ({}) can't {action}. Ask an organization owner.",
            self.member.role.as_str()
        ))
        .into())
    }
}

/// The header a Console request names its organization with on workspace
/// routes (decision 0023).
pub const ORGANIZATION_HEADER: &str = "Zupflow-Organization";

/// A store failure the caller can do nothing about. The store has logged it.
pub fn workspace_unavailable(_: WorkspaceError) -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

/// A Console user acting in one workspace of an organization they're an
/// active member of.
pub struct WorkspaceAccess {
    pub organization: OrganizationAccess,
    pub workspace: Workspace,
}

impl WorkspaceAccess {
    /// Establishes the organization from the request, never from the
    /// workspace: the `Zupflow-Organization` header is required (`400`), the
    /// caller must be an active member of it, and the workspace must be in it.
    /// Every other case answers the same `404`.
    pub async fn resolve(
        state: &AppState,
        user: User,
        headers: &axum::http::HeaderMap,
        workspace: &str,
    ) -> Result<Self, Failure> {
        let named = headers
            .get(ORGANIZATION_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty());
        let Some(organization) = named else {
            return Err(ApiError::new(
                400,
                "organization_required",
                format!("Name the organization this request acts in with the {ORGANIZATION_HEADER} header."),
            )
            .into());
        };
        let organization = OrganizationAccess::resolve_hiding_as(state, user, organization, "Workspace").await?;
        let not_found = || Failure(ApiError::not_found("Workspace"));
        let workspace_id = WorkspaceId::parse(workspace).map_err(|_| not_found())?;
        let workspace = state
            .workspaces
            .find_workspace(organization.organization.id, workspace_id)
            .await
            .map_err(workspace_unavailable)?
            .ok_or_else(not_found)?;
        Ok(Self {
            organization,
            workspace,
        })
    }
}
