//! Idempotency for `POST` (decision 0016): one middleware, the same rules for
//! every route. A key is 1–255 printable ASCII characters, belongs to the
//! caller that sent it, and lasts 24 hours from its first use.
//!
//! The [`IdempotencyStore`] keeps one row per key and caller: running (with a
//! lock that lapses after a minute, so a crashed request's key is taken over)
//! or completed (with the status, and the body unless the route shows a secret
//! once). Store methods take `now`, so expiry and lapsing are testable.

use std::fmt;

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::environment::Environment;
use crate::ids::{ApiKeyId, OrganizationId, UserId, WorkspaceId};
use crate::time::Timestamp;

pub const HEADER: &str = "Idempotency-Key";

/// How long a key lasts from its first use.
pub const KEY_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// How long a request in flight holds its key before another may take it over.
pub const LOCK_MS: i64 = 60 * 1000;

/// 1–255 printable ASCII characters.
pub fn valid_key(key: &str) -> bool {
    (1..=255).contains(&key.len()) && key.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Who sent a key, and in which environment: a member (in a workspace's
/// environment on workspace routes, in none on organization routes) or an API
/// key (always in its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyCaller {
    Member {
        user_id: UserId,
        environment: Option<Environment>,
    },
    ApiKey {
        api_key_id: ApiKeyId,
        environment: Environment,
    },
}

/// Whose key it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyScope {
    /// Every route with an organization: the organization, the workspace on
    /// workspace routes, and the caller.
    Organization {
        organization_id: OrganizationId,
        workspace_id: Option<WorkspaceId>,
        caller: IdempotencyCaller,
    },
    /// Creating an organization, the one `POST` with no organization yet: the
    /// user's own key space.
    User { user_id: UserId },
}

/// Proof that a request holds a key's lock: only it may complete or abandon it.
#[derive(Clone, PartialEq, Eq)]
pub struct LockToken([u8; 16]);

impl LockToken {
    pub fn generate() -> Result<Self, IdempotencyError> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| IdempotencyError::Unavailable)?;
        Ok(Self(bytes))
    }

    /// For row mapping only.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for LockToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LockToken(..)")
    }
}

/// What a request with a key does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begun {
    /// New, or taken over from a request whose lock lapsed: run it.
    Run(LockToken),
    /// Done with the same fingerprint: answer this. A `None` body with a
    /// status other than `204` means the route showed a secret once.
    Replay { status: u16, body: Option<Vec<u8>> },
    /// Another request holds it.
    InProgress,
    /// Used for a different request.
    Reused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdempotencyError {
    #[error("the store is unavailable")]
    Unavailable,
}

#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    /// Starts a request with `key` in `scope`, atomically: a key counts as
    /// unused once [`KEY_TTL_MS`] after its first use, and its scope's expired
    /// rows may be swept here.
    async fn begin(
        &self,
        scope: &IdempotencyScope,
        key: &str,
        fingerprint: &str,
        now: Timestamp,
    ) -> Result<Begun, IdempotencyError>;

    /// Saves the answer, if `lock` still holds the key; otherwise does nothing.
    async fn complete(
        &self,
        scope: &IdempotencyScope,
        key: &str,
        lock: &LockToken,
        status: u16,
        body: Option<Vec<u8>>,
    ) -> Result<(), IdempotencyError>;

    /// Forgets the key (a `5xx`: the retry runs again), if `lock` still holds it.
    async fn abandon(&self, scope: &IdempotencyScope, key: &str, lock: &LockToken) -> Result<(), IdempotencyError>;
}

/// Fingerprints requests under the server's secret key, so a stored
/// fingerprint can't be used to guess a body (which may hold a credential)
/// back. Domain-separated from the key's other uses.
pub struct Fingerprinter {
    key: Zeroizing<[u8; 32]>,
}

impl Fingerprinter {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key: Zeroizing::new(key),
        }
    }

    /// `POST {template} {hmac}`: the route's template in the clear, and the
    /// actual path, query and body only inside the keyed digest, because some
    /// paths carry a reference that is itself a credential.
    pub fn fingerprint(&self, template: &str, path_and_query: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.key.as_ref()).expect("HMAC takes a key of any length");
        mac.update(b"zupflow.idempotency.v1\0");
        mac.update(path_and_query.as_bytes());
        mac.update(b"\n");
        mac.update(body);
        let digest = mac.finalize().into_bytes();
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("POST {template} {hex}")
    }
}

impl fmt::Debug for Fingerprinter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Fingerprinter(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_printable_ascii_keys() {
        assert!(valid_key("a"));
        assert!(valid_key(" ~"));
        assert!(valid_key(&"k".repeat(255)));
        for bad in ["", "tab\there", "é", "\u{7f}", &"k".repeat(256)] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn fingerprints_the_actual_request_under_the_key() {
        let fingerprinter = Fingerprinter::new([1; 32]);
        let one = fingerprinter.fingerprint("/v1/x/{id}", "/v1/x/1?a=b", b"{}");
        assert!(one.starts_with("POST /v1/x/{id} "));
        assert!(!one.contains("/v1/x/1"), "the actual path stays inside the digest");
        assert_eq!(one, fingerprinter.fingerprint("/v1/x/{id}", "/v1/x/1?a=b", b"{}"));
        for (path, body) in [
            ("/v1/x/2?a=b", &b"{}"[..]),
            ("/v1/x/1?a=c", b"{}"),
            ("/v1/x/1?a=b", b"{ }"),
        ] {
            assert_ne!(
                one,
                fingerprinter.fingerprint("/v1/x/{id}", path, body),
                "{path} {body:?}"
            );
        }
        assert_ne!(
            one,
            Fingerprinter::new([2; 32]).fingerprint("/v1/x/{id}", "/v1/x/1?a=b", b"{}")
        );
    }
}
