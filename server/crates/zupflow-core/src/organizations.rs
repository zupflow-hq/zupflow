//! Organizations, their members, and the users behind them: the domain's
//! types and its [`OrganizationStore`].
//!
//! Users are global (one person belongs to many organizations); everything
//! else here is organization-scoped, and every store method on it takes the
//! organization first. The few user-keyed reads and changes that span
//! organizations (a user's own memberships, for `/v1/me`, and their own
//! invitations) are named for the user, and reach only the user's own rows.

use async_trait::async_trait;
use serde::Serialize;

use crate::identity::VerifiedIdentity;
use crate::ids::{MemberId, OrganizationId, UserId};
use crate::time::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Owner,
    Admin,
    Developer,
    Viewer,
}

impl MemberRole {
    pub const ALL: [Self; 4] = [Self::Owner, Self::Admin, Self::Developer, Self::Viewer];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Developer => "developer",
            Self::Viewer => "viewer",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == value)
    }

    /// Owners and admins: organization settings, inviting and removing members.
    pub fn is_admin(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }

    /// Everyone but viewers: building with apps, installations and agents.
    pub fn is_developer(self) -> bool {
        !matches!(self, Self::Viewer)
    }

    /// Whether a member with this role can grant `role` by inviting someone:
    /// owners and admins, and only owners an owner invitation.
    pub fn can_grant(self, role: MemberRole) -> bool {
        self.is_admin() && (role != Self::Owner || self == Self::Owner)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberStatus {
    Active,
    Invited,
}

impl MemberStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Invited => "invited",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "invited" => Some(Self::Invited),
            _ => None,
        }
    }
}

/// The contract's `User`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: UserId,
    pub name: String,
    pub email: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    pub id: OrganizationId,
    pub name: String,
    pub slug: String,
    pub created_at: Timestamp,
}

/// The contract's `OrganizationSetup`: what an organization has done so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct OrganizationSetup {
    pub has_app: bool,
    pub has_ready_configuration: bool,
    pub has_workspace: bool,
    pub has_api_key: bool,
    pub has_installation: bool,
    pub has_agent: bool,
    pub has_run: bool,
}

/// How long an invitation can be accepted after it was sent: the contract's
/// `INVITATION_TTL_DAYS`, 7 days. An expired one can still be declined.
pub const INVITATION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// `GET /v1/me/invitations` lists at most this many: the contract's `INVITATION_LIST_LIMIT`.
pub const INVITATION_LIST_LIMIT: u32 = 50;

/// An invitation expired longer ago than this is left out of the invitee's
/// list: the contract's `EXPIRED_INVITATION_SHOWN_DAYS`, 30 days.
pub const EXPIRED_INVITATION_SHOWN_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// When an invitation sent at `invited_at` stops being accepted.
pub fn invitation_expires_at(invited_at: Timestamp) -> Timestamp {
    Timestamp::from_millis(invited_at.millis().saturating_add(INVITATION_TTL_MS))
}

/// The contract's `Member`: a membership with its user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    pub id: MemberId,
    pub organization_id: OrganizationId,
    pub user: User,
    pub role: MemberRole,
    pub status: MemberStatus,
    pub invited_at: Timestamp,
    /// An invitation's expiry; `None` once it's an active membership.
    pub expires_at: Option<Timestamp>,
    pub joined_at: Option<Timestamp>,
    pub last_active_at: Option<Timestamp>,
}

/// The contract's `Inviter`: who sent an invitation, as its invitee sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inviter {
    pub name: String,
    pub email: String,
}

/// An invitation as its invitee sees it: the organization, the role it
/// grants and who sent it, never the organization's members.
///
/// An invitation grants only what its grantor (who sent it, or last changed
/// its role) can still grant: they're an active member of its organization
/// whose role [can grant](MemberRole::can_grant) it. Removed or demoted below
/// that, their invitations are withdrawn in the same transaction; one whose
/// grantor isn't known (sent before grantors were recorded, or the user is
/// gone) grants nothing, and is answered like a withdrawn one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    /// The membership it becomes once accepted.
    pub id: MemberId,
    pub organization: Organization,
    pub role: MemberRole,
    /// The grantor.
    pub invited_by: Inviter,
    pub invited_at: Timestamp,
    pub expires_at: Timestamp,
}

impl Invitation {
    pub fn is_expired(&self, now: Timestamp) -> bool {
        self.expires_at <= now
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("the store is unavailable")]
    Unavailable,
    #[error("that slug is taken")]
    SlugTaken,
    #[error("already a member or invited")]
    AlreadyMember,
    #[error("an organization needs at least one active owner")]
    LastOwner,
    #[error("not found")]
    NotFound,
    /// The acting member's role, read inside the change's transaction, doesn't
    /// allow it: they aren't an active member any more, or not a high enough role.
    #[error("the acting member's role doesn't allow that")]
    NotAllowed,
    /// A new identity whose email the provider hasn't verified. It neither
    /// creates nor claims a user.
    #[error("the identity's email isn't verified")]
    EmailUnverified,
    /// A new identity whose email belongs to a user already linked to another
    /// identity. Only an invitee nobody has signed in as can be claimed.
    #[error("that email belongs to a user who signs in another way")]
    EmailInUse,
    /// The invitation was accepted already: it's a membership now.
    #[error("the invitation was already accepted")]
    InvitationAccepted,
    /// The invitation is past its expiry, so it can't be accepted.
    #[error("the invitation has expired")]
    InvitationExpired,
}

#[async_trait]
pub trait OrganizationStore: Send + Sync {
    /// The user an identity signs in as. A linked identity is its user, whose
    /// name and avatar follow the profile. A new identity needs a verified
    /// email ([`StoreError::EmailUnverified`]); then it claims the user with
    /// that email if nobody signs in as them yet (an invitee), is refused if
    /// somebody does ([`StoreError::EmailInUse`]), and otherwise creates a new
    /// user. Not tenant data: users are global.
    async fn user_for_identity(&self, identity: &VerifiedIdentity) -> Result<User, StoreError>;

    /// The user's active memberships, with their organizations. Spans
    /// organizations by design: it's the user's own list, for `/v1/me`.
    async fn list_active_memberships_of_user(&self, user_id: UserId)
    -> Result<Vec<(Organization, Member)>, StoreError>;

    /// The organization with the user's active membership in it, or `None`
    /// when either is missing. Records the member as active now.
    async fn find_active_membership(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<(Organization, Member)>, StoreError>;

    /// A new organization whose only member is its owner, `user_id`.
    async fn create_organization(
        &self,
        organization_id: OrganizationId,
        name: &str,
        slug: &str,
        user_id: UserId,
    ) -> Result<(Organization, Member), StoreError>;

    async fn update_organization(
        &self,
        organization_id: OrganizationId,
        name: Option<&str>,
        slug: Option<&str>,
    ) -> Result<Organization, StoreError>;

    async fn load_setup(&self, organization_id: OrganizationId) -> Result<OrganizationSetup, StoreError>;

    /// Members and invitations, oldest first.
    async fn list_members(&self, organization_id: OrganizationId) -> Result<Vec<Member>, StoreError>;

    /// Invites `email` (lowercased), creating a user for it if there's none,
    /// granted by `acting`, whose role is read again in the transaction: an
    /// active member who [can grant](MemberRole::can_grant) `role`, or
    /// [`StoreError::NotAllowed`]. An expired invitation for the address is
    /// renewed (sent now, with this role and grantor); anyone else already in
    /// the organization, invited or not, is [`StoreError::AlreadyMember`].
    async fn invite_member(
        &self,
        organization_id: OrganizationId,
        email: &str,
        role: MemberRole,
        acting: MemberId,
    ) -> Result<Member, StoreError>;

    /// The user's own invitations, pending and expired, that their grantor
    /// can still grant, newest first: at most [`INVITATION_LIST_LIMIT`],
    /// leaving out those expired more than [`EXPIRED_INVITATION_SHOWN_MS`]
    /// before `now`. Spans organizations by design: it's the user's own list.
    /// Nobody else's.
    async fn list_invitations_of_user(&self, user_id: UserId, now: Timestamp) -> Result<Vec<Invitation>, StoreError>;

    /// Accepts the user's own invitation as of `now`: an active membership
    /// with the role the invitation holds when it's accepted. In one
    /// transaction, the invitation is read again, the user's own only
    /// ([`StoreError::NotFound`] for anyone else's, or one withdrawn or
    /// declined), still an invitation ([`StoreError::InvitationAccepted`]),
    /// still granted by its grantor (else withdrawn: [`StoreError::NotFound`])
    /// and not expired ([`StoreError::InvitationExpired`]), so concurrent
    /// accepts, a decline, a withdrawal or the grantor's demotion can't both
    /// succeed.
    async fn accept_invitation_of_user(
        &self,
        user_id: UserId,
        invitation_id: MemberId,
        now: Timestamp,
    ) -> Result<(Organization, Member), StoreError>;

    /// Declines the user's own invitation, expired or not: it's withdrawn.
    /// The same checks as accepting, in one transaction, but expiry.
    async fn decline_invitation_of_user(&self, user_id: UserId, invitation_id: MemberId) -> Result<(), StoreError>;

    /// The member, in this organization only.
    async fn find_member(
        &self,
        organization_id: OrganizationId,
        member_id: MemberId,
    ) -> Result<Option<Member>, StoreError>;

    /// Changes a role, for `acting`, who must be an active owner. Refuses to
    /// leave the organization without an active owner. The acting member's
    /// role, the target and the owners are read and written in one
    /// transaction, so a concurrent demotion can't slip by. On an invitation,
    /// `acting` becomes its grantor; on an active member, the invitations
    /// they granted that the new role can't grant are withdrawn with it.
    async fn update_member_role(
        &self,
        organization_id: OrganizationId,
        member_id: MemberId,
        role: MemberRole,
        acting: MemberId,
    ) -> Result<Member, StoreError>;

    /// Removes a member or withdraws an invitation, for `acting`, who must be
    /// an active owner or admin, and an owner to remove an owner (invited or
    /// active). Never the last active owner. With `only_invited`, an active
    /// member (an invitation accepted in the meantime) is left alone:
    /// [`StoreError::InvitationAccepted`]. Removing an active member
    /// withdraws every invitation they granted. All read and written in one
    /// transaction, like [`OrganizationStore::update_member_role`].
    async fn remove_member(
        &self,
        organization_id: OrganizationId,
        member_id: MemberId,
        acting: MemberId,
        only_invited: bool,
    ) -> Result<(), StoreError>;
}
