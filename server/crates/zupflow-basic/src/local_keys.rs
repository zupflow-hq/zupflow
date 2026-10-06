//! [`LocalKeyService`]: wraps data keys with a 256-bit key from configuration,
//! using AES-256-GCM. The key never leaves the process; a deployment that wants
//! a managed key service plugs in another implementation of the trait.
//!
//! Every wrap uses a random 96-bit nonce, which keeps one key safe for about
//! 2^32 wraps (NIST SP 800-38D); rotate the key well before that, keeping the
//! old one as a previous key so what it wrapped still opens.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;
use zupflow_core::keys::{self, DataKey, EncryptionContext, KeyError, KeyService, WrappedKey};

const NONCE_LENGTH: usize = 12;

struct LocalKey {
    id: String,
    material: Zeroizing<[u8; 32]>,
}

/// Wraps with the current key; still unwraps with keys it was rotated from.
pub struct LocalKeyService {
    current: LocalKey,
    previous: Vec<LocalKey>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LocalKeyError {
    #[error("the key isn't base64")]
    NotBase64,
    #[error("the key must be 32 bytes (256 bits), base64-encoded")]
    WrongLength,
    #[error("a key ID must be 1–64 characters of letters, digits, `-`, `_`, `.` or `:`")]
    InvalidKeyId,
    #[error("each key needs its own ID; this one is already in use")]
    DuplicateKeyId,
}

fn key_id(id: &str) -> Result<String, LocalKeyError> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if valid {
        Ok(id.to_owned())
    } else {
        Err(LocalKeyError::InvalidKeyId)
    }
}

fn decode(encoded: &str) -> Result<Zeroizing<[u8; 32]>, LocalKeyError> {
    let bytes = Zeroizing::new(STANDARD.decode(encoded).map_err(|_| LocalKeyError::NotBase64)?);
    let mut material = Zeroizing::new([0u8; 32]);
    if bytes.len() != material.len() {
        return Err(LocalKeyError::WrongLength);
    }
    material.copy_from_slice(&bytes);
    Ok(material)
}

impl LocalKeyService {
    /// The service with its current key: an ID (stored beside every wrapped key)
    /// and 32 bytes, base64-encoded.
    pub fn new(id: &str, key_base64: &str) -> Result<Self, LocalKeyError> {
        Ok(Self {
            current: LocalKey {
                id: key_id(id)?,
                material: decode(key_base64)?,
            },
            previous: Vec::new(),
        })
    }

    /// The service with a key derived from the server's own secret
    /// (`ZUPFLOW_SECRET_KEY`, decision 0033), under its own domain label so it
    /// is never the key anything else uses. Its ID is derived too, so keys
    /// from two secrets never share one.
    pub fn from_server_secret(secret: &[u8; 32]) -> Self {
        let derive = |label: &[u8]| -> [u8; 32] {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes a key of any length");
            mac.update(label);
            mac.finalize().into_bytes().into()
        };
        let id: String = derive(b"zupflow.keys.id.v1")[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            current: LocalKey {
                id: format!("server-{id}"),
                material: Zeroizing::new(derive(b"zupflow.keys.wrap.v1")),
            },
            previous: Vec::new(),
        }
    }

    /// A key this service was rotated from: still unwraps what it wrapped, never wraps again.
    pub fn with_previous(mut self, id: &str, key_base64: &str) -> Result<Self, LocalKeyError> {
        let id = key_id(id)?;
        // A reused ID would hide one key behind the other, and everything it wrapped would look tampered with.
        if self.key(&id).is_some() {
            return Err(LocalKeyError::DuplicateKeyId);
        }
        self.previous.push(LocalKey {
            id,
            material: decode(key_base64)?,
        });
        Ok(self)
    }

    fn key(&self, id: &str) -> Option<&LocalKey> {
        std::iter::once(&self.current)
            .chain(&self.previous)
            .find(|key| key.id == id)
    }
}

/// The wrapping's authenticated data: the key's ID and the owner's context.
fn aad(key_id: &str, context: &EncryptionContext) -> String {
    format!("zupflow.wrap;key={key_id};{}", context.canonical())
}

#[async_trait]
impl KeyService for LocalKeyService {
    async fn wrap(&self, context: &EncryptionContext, key: &DataKey) -> Result<WrappedKey, KeyError> {
        let nonce = keys::nonce()?;
        let ciphertext = keys::encrypt(
            &self.current.material,
            &nonce,
            aad(&self.current.id, context).as_bytes(),
            key.expose(),
        )?;
        let mut bytes = nonce.to_vec();
        bytes.extend_from_slice(&ciphertext);
        Ok(WrappedKey {
            key_id: self.current.id.clone(),
            bytes,
        })
    }

    async fn unwrap(&self, context: &EncryptionContext, wrapped: &WrappedKey) -> Result<DataKey, KeyError> {
        let Some(key) = self.key(&wrapped.key_id) else {
            // Not secret, and the likeliest cause is a previous key missing from the configuration.
            let key_id: String = wrapped.key_id.chars().take(64).collect();
            tracing::warn!(event = "keys.unknown_key_id", organization_id = %context.organization_id(), %key_id);
            return Err(KeyError::Refused);
        };
        if wrapped.bytes.len() <= NONCE_LENGTH {
            return Err(KeyError::Refused);
        }
        let (nonce, ciphertext) = wrapped.bytes.split_at(NONCE_LENGTH);
        let nonce: [u8; NONCE_LENGTH] = nonce.try_into().map_err(|_| KeyError::Refused)?;
        let plaintext = keys::decrypt(&key.material, &nonce, aad(&key.id, context).as_bytes(), ciphertext)?;
        // Copied straight from the zeroizing plaintext into the zeroizing key.
        DataKey::from_slice(&plaintext).ok_or(KeyError::Refused)
    }
}
