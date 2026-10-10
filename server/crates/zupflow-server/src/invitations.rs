//! `/v1/me/invitations`: the signed-in person's own invitations, and their
//! answer to each. Click to accept (decided 2026-10-10): nobody joins an
//! organization without accepting, and signing in with an invited address
//! joins nothing by itself.
//!
//! Console-only, and there's no member role to check: the person isn't a
//! member yet, and the answer is theirs alone. Instead, an invitation is found
//! by its invitee only, so anyone else's, and one withdrawn or declined,
//! answers the same `404` as one that never existed; so does one its grantor
//! can't grant any more (see [`Invitation`]). Invitations go by email,
//! so only a sign-in that vouches for the user's address now sees or answers
//! one (`403 email_unverified` otherwise, before any ID is read).

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use zupflow_core::error::ApiError;
use zupflow_core::ids::{MemberId, OrganizationId};
use zupflow_core::organizations::{Invitation, Inviter, MemberRole, StoreError, User};
use zupflow_core::time::Timestamp;

use crate::auth::{AppState, ConsoleSession, SignInAddress, unavailable};
use crate::http::{Failure, envelope};
use crate::organizations::view;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/me/invitations", get(list_invitations))
        .route("/v1/me/invitations/{invitation}/accept", post(accept_invitation))
        .route("/v1/me/invitations/{invitation}/decline", post(decline_invitation))
}

/// The contract's `InvitationOrganization`: the organization's name, never its members.
#[derive(Serialize)]
struct OrganizationRef {
    id: OrganizationId,
    name: String,
    slug: String,
}

/// The contract's `Invitation`.
#[derive(Serialize)]
struct InvitationView {
    id: MemberId,
    organization: OrganizationRef,
    role: MemberRole,
    invited_by: Inviter,
    status: &'static str,
    invited_at: Timestamp,
    expires_at: Timestamp,
}

fn invitation_view(invitation: Invitation, now: Timestamp) -> InvitationView {
    InvitationView {
        id: invitation.id,
        status: if invitation.is_expired(now) {
            "expired"
        } else {
            "pending"
        },
        organization: OrganizationRef {
            id: invitation.organization.id,
            name: invitation.organization.name,
            slug: invitation.organization.slug,
        },
        role: invitation.role,
        invited_by: invitation.invited_by,
        invited_at: invitation.invited_at,
        expires_at: invitation.expires_at,
    }
}

/// The user, when this sign-in vouches for their address now.
fn invitee(session: ConsoleSession) -> Result<User, Failure> {
    let message = match session.address {
        SignInAddress::Vouched => return Ok(session.user),
        SignInAddress::Unverified => format!(
            "Invitations go to {}. Verify that address with your sign-in provider, then sign in again.",
            session.user.email
        ),
        // Not "verify it": their provider no longer has it. Nor "get invited
        // at the new one": that invitation is for another user, whom this
        // identity, linked to this one, never becomes.
        SignInAddress::Changed => format!(
            "Invitations go to {email}, the address this account was created with, and your sign-in \
             provider now sends a different one. Only a sign-in that verifies {email} can answer them.",
            email = session.user.email
        ),
    };
    tracing::warn!(event = "invitations.email_unverified");
    Err(ApiError::new(403, "email_unverified", message).into())
}

fn invitation_not_found() -> Failure {
    ApiError::not_found("Invitation").into()
}

fn refused(error: StoreError) -> Failure {
    match error {
        StoreError::NotFound => invitation_not_found(),
        StoreError::InvitationAccepted => {
            ApiError::new(409, "invitation_accepted", "You've already accepted this invitation.").into()
        }
        StoreError::InvitationExpired => ApiError::new(
            422,
            "invitation_expired",
            "This invitation has expired. Ask an admin of the organization to invite you again.",
        )
        .into(),
        error => unavailable(error),
    }
}

async fn list_invitations(State(state): State<AppState>, session: ConsoleSession) -> Result<Response, Failure> {
    let user = invitee(session)?;
    let now = Timestamp::now();
    let invitations: Vec<InvitationView> = state
        .organizations
        .list_invitations_of_user(user.id, now)
        .await
        .map_err(unavailable)?
        .into_iter()
        .map(|invitation| invitation_view(invitation, now))
        .collect();
    Ok(envelope(StatusCode::OK, invitations))
}

async fn accept_invitation(
    State(state): State<AppState>,
    session: ConsoleSession,
    Path(invitation): Path<String>,
) -> Result<Response, Failure> {
    let user = invitee(session)?;
    let invitation_id = MemberId::parse(&invitation).map_err(|_| invitation_not_found())?;
    let (organization, member) = state
        .organizations
        .accept_invitation_of_user(user.id, invitation_id, Timestamp::now())
        .await
        .map_err(refused)?;
    tracing::info!(
        event = "invitations.accepted",
        organization_id = %organization.id,
        member_id = %member.id,
        role = member.role.as_str()
    );
    Ok(envelope(StatusCode::OK, view(&state, organization, member.role).await?))
}

async fn decline_invitation(
    State(state): State<AppState>,
    session: ConsoleSession,
    Path(invitation): Path<String>,
) -> Result<Response, Failure> {
    let user = invitee(session)?;
    let invitation_id = MemberId::parse(&invitation).map_err(|_| invitation_not_found())?;
    state
        .organizations
        .decline_invitation_of_user(user.id, invitation_id)
        .await
        .map_err(refused)?;
    tracing::info!(event = "invitations.declined", member_id = %invitation_id);
    Ok(StatusCode::NO_CONTENT.into_response())
}
