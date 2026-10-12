//! Building and running a server from its [`Config`], apart from storage.
//!
//! The `zupflow` binary opens SQLite and builds its stores, then uses these to
//! do everything else. A deployment with its own implementations of the
//! storage traits does the same from its own binary: it builds an
//! [`AppState`] from its stores and the pieces here, and calls [`serve`].

use std::future::Future;
use std::sync::Arc;

use tokio::net::TcpListener;
use zupflow_core::identity::Identity;
use zupflow_core::models::{ModelCatalog, ModelProvider};
use zupflow_core::runs::store::Durability;
use zupflow_oidc::{OidcConfig, OidcIdentity};

use crate::config::{Config, IdentityMode, ModelProviderMode};
use crate::sign_in::SignInSettings;
use crate::{AppState, RequestGuard};

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("can't set up Console sign-in: {0}")]
    Identity(#[source] zupflow_oidc::OidcConfigError),
    #[error("can't list the configured models")]
    Models,
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

/// What answers runs. No model provider ships with Zupflow (decision 0014):
/// without one, every run fails `model_unavailable`; the scripted model is
/// for local work only.
pub async fn model_provider(config: &Config) -> Result<Arc<dyn ModelProvider>, BootstrapError> {
    match config.model_provider {
        ModelProviderMode::None => {
            tracing::warn!(
                event = "server.no_model_provider",
                "No model provider is configured (ZUPFLOW_MODEL_PROVIDER): every run fails model_unavailable"
            );
            Ok(Arc::new(zupflow_basic::no_model::NoModelProvider))
        }
        ModelProviderMode::Scripted => {
            let served: Vec<String> = config
                .models
                .list_models()
                .await
                .map_err(|_| BootstrapError::Models)?
                .into_iter()
                .map(|model| model.id)
                .collect();
            tracing::warn!(
                event = "server.scripted_model",
                "Runs are answered by the scripted model (ZUPFLOW_MODEL_PROVIDER=scripted): for local work only"
            );
            Ok(Arc::new(zupflow_basic::scripted_model::ScriptedModel::serving(served)))
        }
    }
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
    let served = axum::serve(listener, crate::app(state, guard))
        .with_graceful_shutdown(shutdown)
        .await;
    worker.abort();
    runner.abort();
    deliverer.abort();
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
