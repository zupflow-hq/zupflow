//! [`OidcIdentity`]: Console sign-in through any standard OpenID Connect
//! provider. The Console signs in with the provider and sends its ID token as
//! a bearer; this adapter verifies it against the provider's published keys
//! and vouches for `(issuer, sub)` with the token's profile claims.
//!
//! **What a token must be.** A JWT signed with an asymmetric algorithm
//! (`RS*`, `PS*`, `ES256`, `ES384`; never `none` or `HS*`) by a key in the
//! issuer's key set, with `iss` equal to the configured issuer, `aud`
//! including the configured audience, and `exp`, `sub` and `email` present.
//! `nbf` and `exp` are checked with a minute of leeway.
//!
//! **Keys.** The key set's URL comes from configuration or from the issuer's
//! discovery document (whose `issuer` must equal the configured one). Keys are
//! cached for an hour. An unknown key ID fetches the set again, at most once
//! every 30 seconds, so tokens naming made-up keys can't make us hammer the
//! provider. If a refresh fails, the keys already held keep working.
//!
//! **Where it fetches from.** The issuer is the operator's configuration, not
//! a caller's input, and a self-hoster's provider may well be on a private
//! network, so these fetches don't go through the outbound path that refuses
//! private addresses. They do require HTTPS (plain HTTP only on loopback, for
//! development), follow no redirects, time out after 10 seconds and read at
//! most 256 KiB.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use reqwest::Url;
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};
use zupflow_core::identity::{Identity, IdentityError, Profile, VerifiedIdentity};

/// Signature algorithms a token may use: asymmetric only.
const ALLOWED: [Algorithm; 8] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

const KEYS_TTL: Duration = Duration::from_secs(60 * 60);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DOCUMENT_BYTES: usize = 256 << 10;
const LEEWAY_SECONDS: u64 = 60;

#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// The issuer, exactly as it appears in tokens' `iss`.
    pub issuer: String,
    /// The audience tokens must carry: the Console's client ID.
    pub audience: String,
    /// The key set's URL; found through discovery when `None`.
    pub jwks_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OidcConfigError {
    #[error("ZUPFLOW_OIDC_ISSUER must be an https:// URL (http:// only on localhost)")]
    InvalidIssuer,
    #[error("ZUPFLOW_OIDC_JWKS_URL must be an https:// URL (http:// only on localhost)")]
    InvalidJwksUrl,
    #[error("ZUPFLOW_OIDC_AUDIENCE must not be empty")]
    EmptyAudience,
    #[error("can't build the HTTP client")]
    Client,
}

/// A key from the set, with the family of algorithms it can verify and the
/// algorithms its `alg`, if any, allows.
struct Key {
    decoding: DecodingKey,
    family: Family,
    published: Published,
}

/// What a key's `alg` says it may verify.
enum Published {
    /// No `alg`: any algorithm of its family.
    Any,
    /// Exactly this one.
    Only(Algorithm),
    /// One we don't verify with (encryption, symmetric, unknown): nothing.
    Never,
}

fn published(algorithm: Option<KeyAlgorithm>) -> Published {
    let only = match algorithm {
        None => return Published::Any,
        Some(KeyAlgorithm::RS256) => Algorithm::RS256,
        Some(KeyAlgorithm::RS384) => Algorithm::RS384,
        Some(KeyAlgorithm::RS512) => Algorithm::RS512,
        Some(KeyAlgorithm::PS256) => Algorithm::PS256,
        Some(KeyAlgorithm::PS384) => Algorithm::PS384,
        Some(KeyAlgorithm::PS512) => Algorithm::PS512,
        Some(KeyAlgorithm::ES256) => Algorithm::ES256,
        Some(KeyAlgorithm::ES384) => Algorithm::ES384,
        Some(_) => return Published::Never,
    };
    Published::Only(only)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Rsa,
    Ec,
}

fn family(algorithm: Algorithm) -> Option<Family> {
    match algorithm {
        Algorithm::RS256
        | Algorithm::RS384
        | Algorithm::RS512
        | Algorithm::PS256
        | Algorithm::PS384
        | Algorithm::PS512 => Some(Family::Rsa),
        Algorithm::ES256 | Algorithm::ES384 => Some(Family::Ec),
        _ => None,
    }
}

/// The longest issuer: it's stored as each identity's provider, which the
/// database bounds at 300 characters.
const MAX_ISSUER: usize = 300;

/// The longest email a user can have.
const MAX_EMAIL: usize = 320;

#[derive(Default)]
struct Keys {
    /// By key ID; a key without one is kept under the empty string.
    by_id: HashMap<String, Key>,
    fetched_at: Option<Instant>,
    attempted_at: Option<Instant>,
}

pub struct OidcIdentity {
    config: OidcConfig,
    jwks_url: Option<Url>,
    keys_ttl: Duration,
    min_refresh_interval: Duration,
    client: reqwest::Client,
    keys: RwLock<Keys>,
    /// One refresh at a time.
    refreshing: Mutex<()>,
}

/// An `https://` URL, or `http://` on a loopback host for development.
fn secure_url(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let ok = match url.scheme() {
        "https" => true,
        "http" => loopback,
        _ => false,
    };
    (ok && url.username().is_empty() && url.password().is_none() && url.fragment().is_none()).then_some(url)
}

impl OidcIdentity {
    pub fn new(config: OidcConfig) -> Result<Self, OidcConfigError> {
        if config.issuer.len() > MAX_ISSUER {
            return Err(OidcConfigError::InvalidIssuer);
        }
        secure_url(&config.issuer).ok_or(OidcConfigError::InvalidIssuer)?;
        if config.audience.trim().is_empty() {
            return Err(OidcConfigError::EmptyAudience);
        }
        let jwks_url = match &config.jwks_url {
            Some(url) => Some(secure_url(url).ok_or(OidcConfigError::InvalidJwksUrl)?),
            None => None,
        };
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| OidcConfigError::Client)?;
        Ok(Self {
            config,
            jwks_url,
            keys_ttl: KEYS_TTL,
            min_refresh_interval: MIN_REFRESH_INTERVAL,
            client,
            keys: RwLock::new(Keys::default()),
            refreshing: Mutex::new(()),
        })
    }

    /// Shorter cache and refresh limits, so tests needn't wait an hour.
    #[doc(hidden)]
    pub fn with_timing(mut self, keys_ttl: Duration, min_refresh_interval: Duration) -> Self {
        self.keys_ttl = keys_ttl;
        self.min_refresh_interval = min_refresh_interval;
        self
    }

    async fn fetch_json<T: for<'de> Deserialize<'de>>(&self, url: Url, what: &'static str) -> Result<T, IdentityError> {
        let unavailable = |reason: &str| {
            tracing::error!(event = "oidc.fetch_failed", what, reason);
            IdentityError::Unavailable
        };
        let mut response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|_| unavailable("request"))?;
        if !response.status().is_success() {
            return Err(unavailable("status"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable("body"))? {
            if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
                return Err(unavailable("too_large"));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| unavailable("not_json"))
    }

    async fn key_set_url(&self) -> Result<Url, IdentityError> {
        if let Some(url) = &self.jwks_url {
            return Ok(url.clone());
        }
        #[derive(Deserialize)]
        struct Discovery {
            issuer: String,
            jwks_uri: String,
        }
        let base = self.config.issuer.trim_end_matches('/');
        let url = secure_url(&format!("{base}/.well-known/openid-configuration")).ok_or(IdentityError::Unavailable)?;
        let discovery: Discovery = self.fetch_json(url, "discovery").await?;
        // OpenID Connect Discovery §4.3: the document speaks for this issuer only.
        if discovery.issuer != self.config.issuer {
            tracing::error!(event = "oidc.issuer_mismatch", configured = %self.config.issuer, discovered = %discovery.issuer);
            return Err(IdentityError::Unavailable);
        }
        secure_url(&discovery.jwks_uri).ok_or_else(|| {
            tracing::error!(event = "oidc.insecure_jwks_uri");
            IdentityError::Unavailable
        })
    }

    /// Fetches the key set again, unless another request just did or one was
    /// tried less than [`MIN_REFRESH_INTERVAL`] ago.
    async fn refresh(&self, force: bool) -> Result<(), IdentityError> {
        let _one_at_a_time = self.refreshing.lock().await;
        {
            let keys = self.keys.read().await;
            let recent = keys
                .attempted_at
                .is_some_and(|at| at.elapsed() < self.min_refresh_interval);
            let fresh = keys.fetched_at.is_some_and(|at| at.elapsed() < self.keys_ttl);
            if recent || (fresh && !force) {
                return Ok(());
            }
        }
        self.keys.write().await.attempted_at = Some(Instant::now());
        let url = self.key_set_url().await?;
        let set: JwkSet = self.fetch_json(url, "jwks").await?;
        let mut by_id = HashMap::new();
        for jwk in &set.keys {
            if matches!(&jwk.common.public_key_use, Some(usage) if *usage != PublicKeyUse::Signature) {
                continue;
            }
            let family = match &jwk.algorithm {
                AlgorithmParameters::RSA(_) => Family::Rsa,
                AlgorithmParameters::EllipticCurve(_) => Family::Ec,
                // Symmetric and other keys never verify a sign-in.
                _ => continue,
            };
            let Ok(decoding) = DecodingKey::from_jwk(jwk) else {
                continue;
            };
            let published = published(jwk.common.key_algorithm);
            by_id.insert(
                jwk.common.key_id.clone().unwrap_or_default(),
                Key {
                    decoding,
                    family,
                    published,
                },
            );
        }
        let mut keys = self.keys.write().await;
        keys.by_id = by_id;
        keys.fetched_at = Some(Instant::now());
        tracing::info!(event = "oidc.keys_fetched", count = keys.by_id.len());
        Ok(())
    }

    /// The token's claims if a held key verifies it: the key `kid` names, or,
    /// as OIDC Core §10.1 allows a token to leave `kid` out, any held key.
    async fn verify_with_held_keys(&self, token: &str, algorithm: Algorithm, kid: Option<&str>) -> Option<Claims> {
        let keys = self.keys.read().await;
        match kid {
            Some(kid) => keys.by_id.get(kid).and_then(|key| self.decode(token, algorithm, key)),
            None => keys.by_id.values().find_map(|key| self.decode(token, algorithm, key)),
        }
    }

    /// Checks the token against `key` and returns its claims.
    fn decode(&self, token: &str, algorithm: Algorithm, key: &Key) -> Option<Claims> {
        if family(algorithm) != Some(key.family) {
            return None;
        }
        // A key published for one algorithm verifies only that one.
        match key.published {
            Published::Any => {}
            Published::Only(only) if only == algorithm => {}
            Published::Only(_) | Published::Never => return None,
        }
        let mut validation = Validation::new(algorithm);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_audience(&[&self.config.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = LEEWAY_SECONDS;
        jsonwebtoken::decode::<Claims>(token, &key.decoding, &validation)
            .ok()
            .map(|data| data.claims)
    }
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    email: Option<String>,
    /// A boolean, or the string `"true"` some providers send.
    email_verified: Option<serde_json::Value>,
    name: Option<String>,
    picture: Option<String>,
}

impl Claims {
    fn email_verified(&self) -> bool {
        match &self.email_verified {
            Some(serde_json::Value::Bool(verified)) => *verified,
            Some(serde_json::Value::String(verified)) => verified == "true",
            _ => false,
        }
    }
}

#[async_trait]
impl Identity for OidcIdentity {
    async fn verify(&self, bearer: Option<&str>) -> Result<VerifiedIdentity, IdentityError> {
        let token = bearer.ok_or(IdentityError::Unauthenticated)?;
        let header = jsonwebtoken::decode_header(token).map_err(|_| IdentityError::Unauthenticated)?;
        if !ALLOWED.contains(&header.alg) {
            return Err(IdentityError::Unauthenticated);
        }
        let kid = header.kid;

        // Keys are fetched before the first sign-in and again when they're old;
        // a failure here leaves the keys we have in use.
        let stale = self
            .keys
            .read()
            .await
            .fetched_at
            .is_none_or(|at| at.elapsed() >= self.keys_ttl);
        if stale && let Err(error) = self.refresh(false).await {
            if self.keys.read().await.fetched_at.is_none() {
                return Err(error);
            }
            tracing::warn!(event = "oidc.using_cached_keys");
        }
        let known = match &kid {
            Some(kid) => self.keys.read().await.by_id.contains_key(kid),
            None => true,
        };
        if !known {
            // Possibly rotated: look again, within the refresh limit.
            let _ = self.refresh(true).await;
        }
        // Never fetched (the provider was down from the start): that's an
        // outage, not a bad token.
        if self.keys.read().await.fetched_at.is_none() {
            return Err(IdentityError::Unavailable);
        }

        let mut claims = self.verify_with_held_keys(token, header.alg, kid.as_deref()).await;
        if claims.is_none() && kid.is_none() {
            // A provider that leaves out `kid` can't announce a rotation by
            // naming a new key, so a token no held key verifies looks again,
            // within the same refresh limit.
            let _ = self.refresh(true).await;
            claims = self.verify_with_held_keys(token, header.alg, None).await;
        }
        let claims = claims.ok_or(IdentityError::Unauthenticated)?;
        let Some(email) = claims
            .email
            .as_deref()
            .map(str::trim)
            .filter(|email| !email.is_empty() && email.len() <= MAX_EMAIL)
        else {
            tracing::warn!(
                event = "oidc.unusable_email",
                reason = if claims.email.is_some() {
                    "blank_or_too_long"
                } else {
                    "missing"
                }
            );
            return Err(IdentityError::Unauthenticated);
        };
        if claims.sub.is_empty() || claims.sub.len() > 300 {
            return Err(IdentityError::Unauthenticated);
        }
        Ok(VerifiedIdentity {
            provider: self.config.issuer.clone(),
            subject: claims.sub.clone(),
            profile: Profile {
                name: claims.name.clone().unwrap_or_default(),
                email: email.to_lowercase(),
                email_verified: claims.email_verified(),
                avatar_url: claims.picture.clone().filter(|url| secure_url(url).is_some()),
            },
        })
    }
}
