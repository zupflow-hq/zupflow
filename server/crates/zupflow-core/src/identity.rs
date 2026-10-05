//! Console sign-in. An [`Identity`] adapter only proves who someone is: a
//! verified `(provider, subject)` and a profile. Zupflow maps that to its own
//! user, and owns memberships and roles; no provider decides access.
//!
//! The open-source build ships `local` (no sign-in: one built-in owner, only
//! on a loopback listener) and, later, `oidc`. API keys and Connect sessions
//! never go through this trait.

use async_trait::async_trait;

/// Someone an adapter vouches for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    /// Which adapter vouches, e.g. `local` or the OIDC issuer.
    pub provider: String,
    /// The provider's stable ID for them. Never an email.
    pub subject: String,
    pub profile: Profile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// Lowercased.
    pub email: String,
    /// Whether the provider verified the email. Only a verified email can
    /// claim a user Zupflow already has under that email (an invitee).
    pub email_verified: bool,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// No credential, or one this adapter doesn't accept.
    #[error("not signed in")]
    Unauthenticated,
    #[error("the identity provider is unavailable")]
    Unavailable,
}

#[async_trait]
pub trait Identity: Send + Sync {
    /// Verifies a Console request's bearer token, if it sent one.
    async fn verify(&self, bearer: Option<&str>) -> Result<VerifiedIdentity, IdentityError>;
}
