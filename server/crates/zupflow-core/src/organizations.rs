//! Organizations, their members, and the users behind them: the domain's
//! types and its [`OrganizationStore`].
//!
//! Users are global (one person belongs to many organizations); everything
//! else here is organization-scoped, and every store method on it takes the
//! organization first. The few user-keyed reads that span organizations
//! (a user's own memberships, for `/v1/me`) are named for the user.

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

/// The contract's `Member`: a membership with its user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    pub id: MemberId,
    pub organization_id: OrganizationId,
    pub user: User,
    pub role: MemberRole,
    pub status: MemberStatus,
    pub invited_at: Timestamp,
    pub joined_at: Option<Timestamp>,
    pub last_active_at: Option<Timestamp>,
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

    /// Invites `email` (lowercased), creating a user for it if there's none.
    async fn invite_member(
        &self,
        organization_id: OrganizationId,
        email: &str,
        role: MemberRole,
    ) -> Result<Member, StoreError>;

    /// The member, in this organization only.
    async fn find_member(
        &self,
        organization_id: OrganizationId,
        member_id: MemberId,
    ) -> Result<Option<Member>, StoreError>;

    /// Changes a role, for `acting`, who must be an active owner. Refuses to
    /// leave the organization without an active owner. The acting member's
    /// role, the target and the owners are read and written in one
    /// transaction, so a concurrent demotion can't slip by.
    async fn update_member_role(
        &self,
        organization_id: OrganizationId,
        member_id: MemberId,
        role: MemberRole,
        acting: MemberId,
    ) -> Result<Member, StoreError>;

    /// Removes a member or withdraws an invitation, for `acting`, who must be
    /// an active owner or admin, and an owner to remove an owner (invited or
    /// active). Never the last active owner. All read and written in one
    /// transaction, like [`OrganizationStore::update_member_role`].
    async fn remove_member(
        &self,
        organization_id: OrganizationId,
        member_id: MemberId,
        acting: MemberId,
    ) -> Result<(), StoreError>;
}
