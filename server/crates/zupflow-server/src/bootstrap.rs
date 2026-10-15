//! Building and running a server from its [`Config`], apart from storage.
//!
//! The `zupflow` binary opens SQLite and builds its stores, then uses these to
//! do everything else. A deployment with its own implementations of the
//! storage traits does the same from its own binary: it builds an
//! [`AppState`] from its stores and the pieces here, and calls [`serve`].

use std::future::Future;
use std::sync::Arc;

use tokio::net::TcpListener;
use zupflow_core::channels::provision::AppProvisioner;
use zupflow_core::identity::Identity;
use zupflow_core::model_providers::ModelProviderCatalog;
use zupflow_core::models::ModelProvider;
use zupflow_core::outbound::OutboundHttp;
use zupflow_core::runs::store::Durability;
use zupflow_oidc::{OidcConfig, OidcIdentity};

use crate::config::{ChannelProvisionerMode, Config, IdentityMode, ModelProviderMode};
use crate::sign_in::SignInSettings;
use crate::{AppState, RequestGuard};

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("can't set up Console sign-in: {0}")]
    Identity(#[source] zupflow_oidc::OidcConfigError),
    #[error("can't build the outbound HTTP client: {0}")]
    Outbound(String),
    /// A run's transitions must survive a power cut once applied (decision
    /// 0046), so a run store that can't promise that is refused.
    #[error("the run store doesn't make each transition durable before it returns")]
    RunStoreNotDurable,
    #[error("can't listen on {address}: {source}")]
    Bind {
        address: std::net::SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("the server stopped: {0}")]
    Serve(#[source] std::io::Error),
}

/// Who signs in to the Console, as configured.
pub fn identity(config: &Config) -> Result<Arc<dyn Identity>, BootstrapError> {
    match &config.identity {
        #[cfg(feature = "local-identity")]
        IdentityMode::Local => {
            tracing::warn!(
                event = "server.local_identity",
                "Console sign-in is off: every Console request is the built-in owner"
            );
            Ok(Arc::new(zupflow_basic::local_identity::LocalIdentity))
        }
        IdentityMode::Oidc {
            issuer,
            audience,
            jwks_url,
        } => {
            let oidc = OidcIdentity::new(OidcConfig {
                issuer: issuer.clone(),
                audience: audience.clone(),
                jwks_url: jwks_url.clone(),
            })
            .map_err(BootstrapError::Identity)?;
            Ok(Arc::new(oidc))
        }
    }
}

/// What the Console is told about signing in.
pub fn sign_in(config: &Config) -> SignInSettings {
    match &config.identity {
        #[cfg(feature = "local-identity")]
        IdentityMode::Local => SignInSettings::Local,
        IdentityMode::Oidc { issuer, audience, .. } => SignInSettings::Oidc {
            issuer: issuer.clone(),
            client_id: audience.clone(),
        },
    }
}

/// The request guard: which origins and hosts may call.
pub fn guard(config: &Config) -> RequestGuard {
    let guard = match config.identity {
        #[cfg(feature = "local-identity")]
        IdentityMode::Local => RequestGuard::local(config.listen.port(), &config.allowed_origins),
        IdentityMode::Oidc { .. } => RequestGuard::with_origins(&config.allowed_origins),
    }
    // The hosted Connect page calls the API from the server's own public origin.
    .with_public_url(&config.public_url);
    match &config.console_origin {
        Some(origin) => {
            tracing::info!(event = "server.console_origin", origin = %origin);
            guard.with_console(origin)
        }
        None => guard,
    }
}

/// What answers runs (decision 0049): the provider adapters, through
/// `outbound`, each turn with its run's connection's key; or the scripted
/// model, which serves every model the scripted providers list that can
/// call tools, for local work only; or, unset, nothing: every run fails
/// `model_unavailable`.
pub fn model_provider(config: &Config, outbound: &Arc<dyn OutboundHttp>) -> Arc<dyn ModelProvider> {
    match config.model_provider {
        ModelProviderMode::None => {
            tracing::warn!(
                event = "server.no_model_provider",
                "No model provider is configured (ZUPFLOW_MODEL_PROVIDER): every run fails model_unavailable"
            );
            Arc::new(zupflow_basic::no_model::NoModelProvider)
        }
        ModelProviderMode::Providers => {
            tracing::info!(
                event = "server.model_providers",
                "Runs are answered by organizations' model providers (ZUPFLOW_MODEL_PROVIDER=providers)"
            );
            Arc::new(zupflow_providers::HttpModelProvider::new(outbound.clone()))
        }
        ModelProviderMode::Scripted => {
            tracing::warn!(
                event = "server.scripted_model",
                "Runs are answered by the scripted model (ZUPFLOW_MODEL_PROVIDER=scripted): for local work only"
            );
            Arc::new(zupflow_basic::scripted_model::ScriptedModel::serving(
                zupflow_basic::scripted_providers::ScriptedProviders::tool_models(),
            ))
        }
    }
}

/// What checks organizations' provider keys and lists their models
/// (decision 0049): the provider adapters through `outbound`, the scripted
/// providers for local work, or, unset, nothing, so no connection can be
/// made.
pub fn model_catalog(config: &Config, outbound: &Arc<dyn OutboundHttp>) -> Arc<dyn ModelProviderCatalog> {
    match config.model_provider {
        ModelProviderMode::None => Arc::new(zupflow_basic::no_providers::NoProviders),
        ModelProviderMode::Providers => Arc::new(zupflow_providers::HttpProviderCatalog::new(outbound.clone())),
        ModelProviderMode::Scripted => Arc::new(zupflow_basic::scripted_providers::ScriptedProviders),
    }
}

/// What makes agents' apps in linked teams: the scripted provisioner for
/// local work, or, unset, nothing, so no team can be linked.
pub fn channel_provisioner(config: &Config) -> Arc<dyn AppProvisioner> {
    match config.channel_provisioner {
        ChannelProvisionerMode::None => {
            tracing::info!(
                event = "server.no_channel_provisioner",
                "No channel provisioner is configured (ZUPFLOW_CHANNEL_PROVISIONER): teams can't be linked"
            );
            Arc::new(zupflow_basic::scripted_channel_provisioner::NoChannelProvisioner)
        }
        ChannelProvisionerMode::Scripted => {
            tracing::warn!(
                event = "server.scripted_channel_provisioner",
                "Channel apps are made by the scripted provisioner (ZUPFLOW_CHANNEL_PROVISIONER=scripted): \
                 for local work only"
            );
            Arc::new(zupflow_basic::scripted_channel_provisioner::ScriptedChannelProvisioner::default())
        }
    }
}

/// What reads deliveries to agents' apps' inbound endpoints: deliveries in
/// Slack's format, signed as Slack signs them. It holds no credential and
/// calls no one; each delivery is checked with its own app's signing secret.
pub fn channel_provider() -> Arc<dyn zupflow_core::channels::provider::ChannelProvider> {
    Arc::new(zupflow_channels::fake::FakeChannelProvider)
}

/// The outbound HTTP client every call to a provider, an MCP server or a
/// webhook endpoint goes through.
pub fn outbound() -> Result<zupflow_outbound::SafeHttpClient, BootstrapError> {
    zupflow_outbound::SafeHttpClient::new().map_err(|error| BootstrapError::Outbound(error.to_string()))
}

/// Serves the API on `config.listen` with the background workers in the same
/// process, until `shutdown` finishes; in-flight requests finish first.
pub async fn serve(
    config: &Config,
    state: AppState,
    guard: RequestGuard,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), BootstrapError> {
    if state.runs.durability() != Durability::Synchronous {
        return Err(BootstrapError::RunStoreNotDurable);
    }
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|source| BootstrapError::Bind {
            address: config.listen,
            source,
        })?;
    tracing::info!(event = "server.listening", address = %config.listen);
    let worker = tokio::spawn(crate::discovery::run_worker(state.clone()));
    let runner = tokio::spawn(crate::run_engine::run_worker(state.clone()));
    let deliverer = tokio::spawn(crate::webhook_delivery::run_workers(state.clone()));
    let provisioner = tokio::spawn(crate::channels::run_worker(state.clone()));
    let inbound = tokio::spawn(crate::channel_inbound::run_worker(state.clone()));
    let served = axum::serve(listener, crate::app(state, guard))
        .with_graceful_shutdown(shutdown)
        .await;
    worker.abort();
    runner.abort();
    deliverer.abort();
    provisioner.abort();
    inbound.abort();
    served.map_err(BootstrapError::Serve)
}

/// Finishes on Ctrl-C or SIGTERM.
pub async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            // Without the handler, wait for SIGTERM rather than shutting down at once.
            tracing::warn!(event = "server.interrupt_unavailable", %error);
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
}
