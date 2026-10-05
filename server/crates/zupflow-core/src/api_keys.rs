//! API keys: how an organization's backend calls the API. A key belongs to
//! one organization and one environment, is shown once, and is stored only as
//! a keyed hash (HMAC-SHA256 under a server-held key) with a public prefix and
//! its last four characters for display.
//!
//! A secret looks like `zf_test_` or `zf_live_`, a four-character selector
//! shown in the prefix, and 40 random base62 characters. The last four are
//! shown too, which leaves 36 secret ones: about 214 bits.

use std::fmt;

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::environment::Environment;
use crate::ids::{ApiKeyId, OrganizationId, UserId};
use crate::organizations::User;
use crate::time::Timestamp;

const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const SELECTOR_LENGTH: usize = 4;
const RANDOM_LENGTH: usize = 40;

/// Every key's secret starts with this.
pub const SECRET_PREFIX: &str = "zf_";

/// The contract's `ApiKey`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKey {
    pub id: ApiKeyId,
    pub organization_id: OrganizationId,
    pub environment: Environment,
    pub name: String,
    /// `zf_test_` or `zf_live_` and the selector: enough to recognize a key.
    pub prefix: String,
    pub last4: String,
    pub created_by: User,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
}

/// A secret just generated: shown to its creator once, never stored.
pub struct NewSecret {
    pub secret: Zeroizing<String>,
    pub prefix: String,
    pub last4: String,
}

impl fmt::Debug for NewSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewSecret")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

/// `n` random base62 characters, by rejection sampling so each is uniform.
fn random_base62(n: usize) -> Result<String, ApiKeyError> {
    let mut out = String::with_capacity(n);
    let mut buffer = [0u8; 64];
    while out.len() < n {
        getrandom::fill(&mut buffer).map_err(|_| ApiKeyError::Unavailable)?;
        // 248 = 4 × 62: bytes at or above it would favor the first characters.
        for byte in buffer.iter().filter(|byte| **byte < 248) {
            if out.len() == n {
                break;
            }
            out.push(char::from(ALPHABET[usize::from(*byte % 62)]));
        }
    }
    Ok(out)
}

impl NewSecret {
    pub fn generate(environment: Environment) -> Result<Self, ApiKeyError> {
        let label = match environment {
            Environment::Production => "live",
            Environment::Sandbox => "test",
        };
        let prefix = format!("{SECRET_PREFIX}{label}_{}", random_base62(SELECTOR_LENGTH)?);
        let secret = Zeroizing::new(format!("{prefix}{}", random_base62(RANDOM_LENGTH)?));
        let last4 = secret[secret.len() - 4..].to_owned();
        Ok(Self { secret, prefix, last4 })
    }
}

/// Hashes secrets under the server's key. Two servers with the same key find
/// the same keys; a changed key orphans every key issued under the old one.
pub struct ApiKeyHasher {
    key: Zeroizing<[u8; 32]>,
}

impl ApiKeyHasher {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key: Zeroizing::new(key),
        }
    }

    /// The stored form of a secret.
    pub fn hash(&self, secret: &str) -> [u8; 32] {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.key.as_ref()).expect("HMAC takes a key of any length");
        mac.update(b"zupflow.api_key.v1\0");
        mac.update(secret.as_bytes());
        mac.finalize().into_bytes().into()
    }
}

impl fmt::Debug for ApiKeyHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKeyHasher(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ApiKeyError {
    #[error("the store is unavailable")]
    Unavailable,
    #[error("not found")]
    NotFound,
    #[error("already revoked")]
    AlreadyRevoked,
}

/// A key to store: everything but the secret itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApiKey {
    pub environment: Environment,
    pub name: String,
    pub prefix: String,
    pub last4: String,
    pub secret_hash: [u8; 32],
    pub created_by: UserId,
}

#[async_trait]
pub trait ApiKeyStore: Send + Sync {
    /// The organization's keys, revoked ones included, newest first.
    async fn list_api_keys(&self, organization_id: OrganizationId) -> Result<Vec<ApiKey>, ApiKeyError>;

    async fn create_api_key(
        &self,
        organization_id: OrganizationId,
        api_key_id: ApiKeyId,
        key: &NewApiKey,
    ) -> Result<ApiKey, ApiKeyError>;

    async fn find_api_key(
        &self,
        organization_id: OrganizationId,
        api_key_id: ApiKeyId,
    ) -> Result<Option<ApiKey>, ApiKeyError>;

    /// Revokes a key for good; [`ApiKeyError::AlreadyRevoked`] the second time.
    async fn revoke_api_key(
        &self,
        organization_id: OrganizationId,
        api_key_id: ApiKeyId,
    ) -> Result<ApiKey, ApiKeyError>;

    /// **Cross-tenant**, by design: a request presents only its key, so this
    /// is where its organization is learned. Finds the unrevoked key with this
    /// hash, from any organization, and records it as used now. Everything
    /// after it is scoped to the key's organization and environment.
    async fn find_active_api_key_by_hash(&self, secret_hash: &[u8; 32]) -> Result<Option<ApiKey>, ApiKeyError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_secrets_in_the_documented_shape() {
        let test = NewSecret::generate(Environment::Sandbox).expect("random bytes");
        assert!(test.secret.starts_with("zf_test_"));
        assert_eq!(test.secret.len(), "zf_test_".len() + 4 + 40);
        assert_eq!(test.prefix.len(), "zf_test_".len() + 4);
        assert!(test.secret.starts_with(&test.prefix));
        assert!(test.secret.ends_with(&test.last4));
        let live = NewSecret::generate(Environment::Production).expect("random bytes");
        assert!(live.secret.starts_with("zf_live_"));
        assert_ne!(test.secret.as_str(), live.secret.as_str());
        assert!(
            test.secret["zf_test_".len()..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric()),
            "base62 after the label"
        );
        assert!(
            !format!("{test:?}").contains(test.secret.as_str()),
            "Debug never shows the secret"
        );
    }

    #[test]
    fn hashes_under_the_server_key() {
        let hasher = ApiKeyHasher::new([7; 32]);
        assert_eq!(hasher.hash("zf_test_abc"), hasher.hash("zf_test_abc"));
        assert_ne!(hasher.hash("zf_test_abc"), hasher.hash("zf_test_abd"));
        assert_ne!(
            hasher.hash("zf_test_abc"),
            ApiKeyHasher::new([8; 32]).hash("zf_test_abc"),
            "another server key, another hash"
        );
    }
}
