//! Envelope encryption for the secrets Zupflow holds (OAuth client secrets,
//! installation credentials, a Connect session's pending OAuth authorization,
//! webhook signing secrets, model provider keys).
//!
//! Each secret is encrypted with its own random data key, and the data key is
//! wrapped by a [`KeyService`]: a local key in the open-source build, a managed
//! key service in a deployment that plugs one in. The database holds only
//! ciphertext and wrapped keys.
//!
//! Both layers are bound to an [`EncryptionContext`] naming the owner, so a
//! secret can't be opened under another tenant's context, even by code that
//! holds the right key. AES state (key schedules, GHASH keys) is wiped after
//! use through aes-gcm's `zeroize` feature, and every key and plaintext this
//! module hands out is wiped on drop.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use async_trait::async_trait;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::apps::ClientRegistrationId;
use crate::environment::Environment;
use crate::ids::{
    AppId, ConnectSessionId, InstallationId, ModelConnectionId, OrganizationId, WebhookEndpointId, WorkspaceId,
};

/// What a secret is for, implied by the constructor that built its context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    OauthClientSecret,
    InstallationCredential,
    ConnectAuthorization,
    WebhookSigningSecret,
    ModelProviderKey,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::OauthClientSecret => "oauth_client_secret",
            Self::InstallationCredential => "installation_credential",
            Self::ConnectAuthorization => "connect_authorization",
            Self::WebhookSigningSecret => "webhook_signing_secret",
            Self::ModelProviderKey => "model_provider_key",
        }
    }
}

/// Who a secret belongs to: the organization, and the workspace, environment,
/// app, owning object and version as they apply. Built from the trust context,
/// never from input, and bound into both the wrapped key and the ciphertext.
///
/// There's one constructor per kind of secret, so none can be built without
/// the parts its kind requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionContext {
    organization_id: OrganizationId,
    workspace_id: Option<WorkspaceId>,
    environment: Environment,
    purpose: Purpose,
    app_id: Option<AppId>,
    object: String,
    version: u32,
}

impl EncryptionContext {
    /// An app's OAuth client secret in one environment. `workspace_id` is set
    /// for a workspace's own app, and `None` for an organization's catalog app.
    pub fn oauth_client_secret(
        organization_id: OrganizationId,
        workspace_id: Option<WorkspaceId>,
        environment: Environment,
        app_id: AppId,
        version: u32,
    ) -> Self {
        Self {
            organization_id,
            workspace_id,
            environment,
            purpose: Purpose::OauthClientSecret,
            app_id: Some(app_id),
            object: app_id.to_string(),
            version,
        }
    }

    /// What an MCP server's authorization server issued to the client Zupflow
    /// registered with it (decision 0039): the client secret and the
    /// registration access token, sealed together as one value. The same
    /// kind of secret as [`EncryptionContext::oauth_client_secret`], but
    /// owned by the registration, not the configuration: that constructor's
    /// object is the app and its version the configuration's secret version,
    /// so a registration sealed under it would share a context with the
    /// configured secret of the same version, and two issuers' registrations
    /// with each other. A registration's is its own row, version 1: what it
    /// was issued never changes, and a new issuer is a new registration.
    /// `workspace_id` is set for a workspace's own app.
    pub fn oauth_client_registration(
        organization_id: OrganizationId,
        workspace_id: Option<WorkspaceId>,
        environment: Environment,
        app_id: AppId,
        registration: ClientRegistrationId,
    ) -> Self {
        Self {
            organization_id,
            workspace_id,
            environment,
            purpose: Purpose::OauthClientSecret,
            app_id: Some(app_id),
            // Hex, so it holds no `;` or `=`, and the prefix keeps it apart from any ID.
            object: format!("registration.{}", registration.uuid().simple()),
            version: 1,
        }
    }

    /// An installation's credential: every field of it (access and refresh
    /// tokens, a pasted key) is sealed together as one value per version, so
    /// its parts can't be swapped with each other.
    pub fn installation_credential(
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        environment: Environment,
        app_id: AppId,
        installation_id: InstallationId,
        version: u32,
    ) -> Self {
        Self {
            organization_id,
            workspace_id: Some(workspace_id),
            environment,
            purpose: Purpose::InstallationCredential,
            app_id: Some(app_id),
            object: installation_id.to_string(),
            version,
        }
    }

    /// What a Connect session keeps while its user is at the provider's
    /// consent screen: the PKCE verifier and the link's reference, sealed as
    /// one value per authorization attempt, so an older attempt's value can't
    /// be opened as the current one.
    pub fn connect_authorization(
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        environment: Environment,
        app_id: AppId,
        session_id: ConnectSessionId,
        attempt: u32,
    ) -> Self {
        Self {
            organization_id,
            workspace_id: Some(workspace_id),
            environment,
            purpose: Purpose::ConnectAuthorization,
            app_id: Some(app_id),
            object: session_id.to_string(),
            version: attempt,
        }
    }

    /// A webhook endpoint's signing secret. An endpoint belongs to one
    /// organization and one environment.
    pub fn webhook_signing_secret(
        organization_id: OrganizationId,
        environment: Environment,
        endpoint_id: WebhookEndpointId,
        version: u32,
    ) -> Self {
        Self {
            organization_id,
            workspace_id: None,
            environment,
            purpose: Purpose::WebhookSigningSecret,
            app_id: None,
            object: endpoint_id.to_string(),
            version,
        }
    }

    /// A model connection's provider API key (decision 0049), one value per
    /// key version. A connection belongs to one organization and one
    /// environment, and no workspace or app.
    pub fn model_provider_key(
        organization_id: OrganizationId,
        environment: Environment,
        connection_id: ModelConnectionId,
        version: u32,
    ) -> Self {
        Self {
            organization_id,
            workspace_id: None,
            environment,
            purpose: Purpose::ModelProviderKey,
            app_id: None,
            object: connection_id.to_string(),
            version,
        }
    }

    /// The organization the secret belongs to, for logs.
    pub fn organization_id(&self) -> OrganizationId {
        self.organization_id
    }

    /// The context as authenticated data. IDs (a lowercase prefix and a base62
    /// body), the fixed names and a decimal version hold no `;` or `=`, and `-`
    /// is never an ID, so two different contexts never encode the same.
    pub fn canonical(&self) -> String {
        let or_dash = |value: Option<String>| value.unwrap_or_else(|| "-".to_owned());
        format!(
            "zupflow.v1;org={};ws={};env={};purpose={};app={};object={};version={}",
            self.organization_id,
            or_dash(self.workspace_id.map(|id| id.to_string())),
            self.environment.as_str(),
            self.purpose.as_str(),
            or_dash(self.app_id.map(|id| id.to_string())),
            self.object,
            self.version,
        )
    }
}

/// A 256-bit data key. Wiped from memory when dropped and never printed.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct DataKey([u8; 32]);

impl DataKey {
    /// A fresh key from the operating system's CSPRNG, filled in place.
    pub fn generate() -> Result<Self, KeyError> {
        let mut key = Self([0; 32]);
        getrandom::fill(&mut key.0).map_err(|_| KeyError::Unavailable)?;
        Ok(key)
    }

    /// A key from exactly 32 bytes, copied straight into the zeroizing value.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 32 {
            return None;
        }
        let mut key = Self([0; 32]);
        key.0.copy_from_slice(bytes);
        Some(key)
    }

    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for DataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DataKey(…)")
    }
}

/// A data key as the key service wrapped it, with the ID of the key that did.
/// Opaque ciphertext, but still never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct WrappedKey {
    pub key_id: String,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for WrappedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WrappedKey {{ key_id: {:?}, bytes: {} bytes }}",
            self.key_id,
            self.bytes.len()
        )
    }
}

/// Why a key operation failed. A wrong context, a tampered ciphertext and an
/// unknown key are the same `Refused`: telling them apart would help an
/// attacker more than an operator. Both variants fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("the key service refused to open this under this context")]
    Refused,
    #[error("the key service is unavailable")]
    Unavailable,
}

/// Wraps and unwraps data keys. Implementations: the local key in this
/// repository, managed key services elsewhere. All pass the conformance suite.
#[async_trait]
pub trait KeyService: Send + Sync {
    async fn wrap(&self, context: &EncryptionContext, key: &DataKey) -> Result<WrappedKey, KeyError>;
    async fn unwrap(&self, context: &EncryptionContext, wrapped: &WrappedKey) -> Result<DataKey, KeyError>;
}

/// Secret bytes as they come out of [`open`]: wiped on drop, never printed.
pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBytes(…)")
    }
}

/// A sealed secret: what's stored.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    pub wrapped_key: WrappedKey,
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Sealed {{ {:?}, ciphertext: {} bytes }}",
            self.wrapped_key,
            self.ciphertext.len()
        )
    }
}

/// AES-256-GCM under `key`, authenticating `aad`. Shared by the envelope and
/// a key service implementation; everything else uses [`seal`]. The caller
/// must never reuse a nonce under the same key: take it from [`nonce`].
#[doc(hidden)]
pub fn encrypt(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, KeyError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| KeyError::Unavailable)?;
    cipher
        .encrypt(&Nonce::from(*nonce), Payload { msg: plaintext, aad })
        .map_err(|_| KeyError::Unavailable)
}

#[doc(hidden)]
/// The inverse of [`encrypt`]. Any mismatch (key, nonce, AAD, ciphertext) is `Refused`.
pub fn decrypt(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Zeroizing<Vec<u8>>, KeyError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| KeyError::Unavailable)?;
    cipher
        .decrypt(&Nonce::from(*nonce), Payload { msg: ciphertext, aad })
        .map(Zeroizing::new)
        .map_err(|_| KeyError::Refused)
}

/// A random 96-bit nonce. Under one key, random nonces stay safe for about
/// 2^32 encryptions; a fresh data key per secret keeps the envelope far from
/// that, and a long-lived wrapping key is rotated well before it.
#[doc(hidden)]
pub fn nonce() -> Result<[u8; 12], KeyError> {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).map_err(|_| KeyError::Unavailable)?;
    Ok(bytes)
}

/// Encrypts `plaintext` for its owner: a new data key, wrapped under `context`,
/// and the ciphertext authenticated with the same context.
pub async fn seal(keys: &dyn KeyService, context: &EncryptionContext, plaintext: &[u8]) -> Result<Sealed, KeyError> {
    let data_key = DataKey::generate()?;
    let wrapped_key = keys.wrap(context, &data_key).await?;
    let nonce = nonce()?;
    let ciphertext = encrypt(data_key.expose(), &nonce, context.canonical().as_bytes(), plaintext)?;
    Ok(Sealed {
        wrapped_key,
        nonce,
        ciphertext,
    })
}

/// Decrypts a sealed secret, but only under the context it was sealed for.
pub async fn open(
    keys: &dyn KeyService,
    context: &EncryptionContext,
    sealed: &Sealed,
) -> Result<SecretBytes, KeyError> {
    let data_key = keys.unwrap(context, &sealed.wrapped_key).await?;
    let plaintext = decrypt(
        data_key.expose(),
        &sealed.nonce,
        context.canonical().as_bytes(),
        &sealed.ciphertext,
    )?;
    Ok(SecretBytes(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_every_part_of_the_context() {
        let org = OrganizationId::generate();
        let ws = WorkspaceId::generate();
        let app = AppId::generate();
        let inst = InstallationId::generate();
        let we = WebhookEndpointId::generate();
        assert_eq!(
            EncryptionContext::installation_credential(org, ws, Environment::Sandbox, app, inst, 3).canonical(),
            format!(
                "zupflow.v1;org={org};ws={ws};env=sandbox;purpose=installation_credential;app={app};object={inst};version=3"
            )
        );
        assert_eq!(
            EncryptionContext::oauth_client_secret(org, None, Environment::Production, app, 1).canonical(),
            format!(
                "zupflow.v1;org={org};ws=-;env=production;purpose=oauth_client_secret;app={app};object={app};version=1"
            )
        );
        assert_eq!(
            EncryptionContext::webhook_signing_secret(org, Environment::Production, we, 2).canonical(),
            format!(
                "zupflow.v1;org={org};ws=-;env=production;purpose=webhook_signing_secret;app=-;object={we};version=2"
            )
        );
        let mc = ModelConnectionId::generate();
        assert_eq!(
            EncryptionContext::model_provider_key(org, Environment::Sandbox, mc, 5).canonical(),
            format!("zupflow.v1;org={org};ws=-;env=sandbox;purpose=model_provider_key;app=-;object={mc};version=5")
        );
        let registration = ClientRegistrationId::generate();
        assert_eq!(
            EncryptionContext::oauth_client_registration(org, Some(ws), Environment::Sandbox, app, registration)
                .canonical(),
            format!(
                "zupflow.v1;org={org};ws={ws};env=sandbox;purpose=oauth_client_secret;app={app};object=registration.{};version=1",
                registration.uuid().simple()
            )
        );
        let cs = ConnectSessionId::generate();
        assert_eq!(
            EncryptionContext::connect_authorization(org, ws, Environment::Sandbox, app, cs, 4).canonical(),
            format!(
                "zupflow.v1;org={org};ws={ws};env=sandbox;purpose=connect_authorization;app={app};object={cs};version=4"
            )
        );
    }

    #[test]
    fn never_prints_key_material() {
        let key = DataKey::from_slice(&[7; 32]).expect("32 bytes");
        assert_eq!(format!("{key:?}"), "DataKey(…)");
        assert!(DataKey::from_slice(&[7; 31]).is_none());
        let wrapped = WrappedKey {
            key_id: "local:1".to_owned(),
            bytes: vec![1, 2, 3],
        };
        assert_eq!(
            format!("{wrapped:?}"),
            r#"WrappedKey { key_id: "local:1", bytes: 3 bytes }"#
        );
    }

    /// A key service that hands the data key back as is, so a test can see
    /// what `seal` generated. Never anything but a test.
    struct Transparent;

    #[async_trait]
    impl KeyService for Transparent {
        async fn wrap(&self, context: &EncryptionContext, key: &DataKey) -> Result<WrappedKey, KeyError> {
            Ok(WrappedKey {
                key_id: context.canonical(),
                bytes: key.expose().to_vec(),
            })
        }

        async fn unwrap(&self, context: &EncryptionContext, wrapped: &WrappedKey) -> Result<DataKey, KeyError> {
            if wrapped.key_id != context.canonical() {
                return Err(KeyError::Refused);
            }
            DataKey::from_slice(&wrapped.bytes).ok_or(KeyError::Refused)
        }
    }

    #[tokio::test]
    async fn seals_each_secret_with_a_fresh_data_key_and_nonce() {
        let context = EncryptionContext::webhook_signing_secret(
            OrganizationId::generate(),
            Environment::Sandbox,
            WebhookEndpointId::generate(),
            1,
        );
        let first = seal(&Transparent, &context, b"same secret").await.expect("seals");
        let second = seal(&Transparent, &context, b"same secret").await.expect("seals");
        assert_ne!(
            first.wrapped_key.bytes, second.wrapped_key.bytes,
            "a new data key per secret"
        );
        assert_ne!(first.wrapped_key.bytes, vec![0; 32], "never a fixed key");
        assert_ne!(first.nonce, second.nonce, "a new nonce per secret");
        assert_ne!(first.ciphertext, second.ciphertext, "equal secrets don't look equal");
        let opened = open(&Transparent, &context, &first).await.expect("opens");
        assert_eq!(opened.expose(), b"same secret");
    }
}
