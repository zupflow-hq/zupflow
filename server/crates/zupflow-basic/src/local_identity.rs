//! [`LocalIdentity`]: Console sign-in for one person on their own machine.
//! Every Console request is the same built-in owner, a real user row, so
//! roles, isolation and audit run the same code as with any other sign-in.
//!
//! It proves nothing about who is calling, which is why the server refuses to
//! start with it unless it listens on loopback only.

use async_trait::async_trait;
use zupflow_core::identity::{Identity, IdentityError, Profile, VerifiedIdentity};

/// The built-in user's address: `.invalid` is reserved, so it can never
/// reach anyone or collide with a real person's address.
pub const LOCAL_EMAIL: &str = "owner@local.invalid";

pub struct LocalIdentity;

#[async_trait]
impl Identity for LocalIdentity {
    async fn verify(&self, _bearer: Option<&str>) -> Result<VerifiedIdentity, IdentityError> {
        Ok(VerifiedIdentity {
            provider: "local".to_owned(),
            subject: "owner".to_owned(),
            profile: Profile {
                name: "Local owner".to_owned(),
                email: LOCAL_EMAIL.to_owned(),
                // Nobody can be invited under a reserved address first.
                email_verified: true,
                avatar_url: None,
            },
        })
    }
}
