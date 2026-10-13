//! Reaching a run's model provider as its organization: the connection the
//! run was admitted with, found in the run's own organization and
//! environment, and its key opened only here, for the turn about to be
//! sent, at the key version the model attempt was prepared with (decisions
//! 0049, 0050). The opened key lives in a [`ProviderKey`], wiped when the
//! turn is done, and is never logged.
//!
//! A connection that went fails the run (its models went with it); a key
//! replaced since the attempt was prepared, or a store or key service that
//! didn't answer, is tried again, with the route read again first.

use zupflow_core::environment::Environment;
use zupflow_core::ids::{ModelConnectionId, OrganizationId};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::model_providers::{ProviderKey, ProviderKind};

use crate::auth::AppState;

/// The provider and the key to send a turn with.
pub struct ModelReach {
    pub kind: ProviderKind,
    /// What people call the connection, for a run's failure.
    pub label: String,
    pub key: ProviderKey,
}

/// Why a connection's key can't be opened for this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unopened {
    /// The connection isn't there any more.
    ConnectionGone,
    /// Its key was replaced since the attempt was prepared.
    KeyReplaced,
    /// The store or the key service didn't answer, or the sealed key isn't
    /// one: why.
    Unavailable(&'static str),
}

impl Unopened {
    pub fn why(self) -> &'static str {
        match self {
            Self::ConnectionGone => "connection_gone",
            Self::KeyReplaced => "key_replaced",
            Self::Unavailable(why) => why,
        }
    }
}

/// The run's connection and its key at `key_version`, in the run's own
/// organization and environment, opened under the connection's own
/// `model_provider_key` context.
pub async fn open(
    state: &AppState,
    organization_id: OrganizationId,
    environment: Environment,
    connection_id: ModelConnectionId,
    key_version: u32,
) -> Result<ModelReach, Unopened> {
    let connection = match state
        .model_store
        .find_connection(organization_id, Some(environment), connection_id)
        .await
    {
        Ok(Some(connection)) => connection,
        Ok(None) => return Err(Unopened::ConnectionGone),
        Err(_) => return Err(Unopened::Unavailable("store_unavailable")),
    };
    let stored = match state
        .model_store
        .connection_key(organization_id, environment, connection_id)
        .await
    {
        Ok(Some(stored)) => stored,
        Ok(None) => return Err(Unopened::ConnectionGone),
        Err(_) => return Err(Unopened::Unavailable("store_unavailable")),
    };
    // Earlier versions are forgotten when a key is replaced: the attempt
    // is asked again, prepared with the new one.
    if stored.version != key_version {
        return Err(Unopened::KeyReplaced);
    }
    let context = EncryptionContext::model_provider_key(organization_id, environment, connection_id, stored.version);
    let opened = keys::open(state.keys.as_ref(), &context, &stored.sealed)
        .await
        .map_err(|error| {
            tracing::error!(event = "runs.model_key_open_failed", %organization_id, %connection_id, %error);
            Unopened::Unavailable("key_unavailable")
        })?;
    let key = ProviderKey::from_bytes(opened.expose()).ok_or(Unopened::Unavailable("key_unreadable"))?;
    Ok(ModelReach {
        kind: connection.kind,
        label: connection.label,
        key,
    })
}
