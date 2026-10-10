//! `zupflow`: runs the API server.

use std::process::ExitCode;
use std::sync::Arc;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use zupflow_basic::local_identity::LocalIdentity;
use zupflow_core::api_keys::ApiKeyHasher;
use zupflow_core::idempotency::Fingerprinter;
use zupflow_core::identity::Identity;
use zupflow_core::models::{ModelCatalog, ModelProvider};
use zupflow_oidc::{OidcConfig, OidcIdentity};
use zupflow_server::config::{Config, IdentityMode, ModelProviderMode};
use zupflow_server::sign_in::SignInSettings;
use zupflow_server::{AppState, RequestGuard};
use zupflow_sqlite::{
    Database, SqliteApiKeyStore, SqliteIdempotencyStore, SqliteOrganizationStore, SqliteWorkspaceStore,
};

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            // Logging isn't configured yet, and a bad configuration must be visible.
            eprintln!("zupflow: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Ok(filter) = EnvFilter::try_new(&config.log) else {
        eprintln!("zupflow: ZUPFLOW_LOG isn't a valid log filter, e.g. `info` or `zupflow_server=debug`");
        return ExitCode::FAILURE;
    };
    tracing_subscriber::fmt().json().with_env_filter(filter).init();

    let database = match Database::open(&config.database).await {
        Ok(database) => database,
        Err(error) => {
            tracing::error!(event = "server.database_failed", path = %config.database.display(), error = %error, cause = ?std::error::Error::source(&error));
            return ExitCode::FAILURE;
        }
    };
    let identity: Arc<dyn Identity> = match &config.identity {
        IdentityMode::Local => {
            tracing::warn!(
                event = "server.local_identity",
                "Console sign-in is off: every Console request is the built-in owner"
            );
            Arc::new(LocalIdentity)
        }
        IdentityMode::Oidc {
            issuer,
            audience,
            jwks_url,
        } => {
            let config = OidcConfig {
                issuer: issuer.clone(),
                audience: audience.clone(),
                jwks_url: jwks_url.clone(),
            };
            match OidcIdentity::new(config) {
                Ok(oidc) => Arc::new(oidc),
                Err(error) => {
                    tracing::error!(event = "server.identity_failed", %error);
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let guard = match config.identity {
        IdentityMode::Local => RequestGuard::local(config.listen.port(), &config.allowed_origins),
        IdentityMode::Oidc { .. } => RequestGuard::with_origins(&config.allowed_origins),
    };
    let guard = match &config.console_origin {
        Some(origin) => {
            tracing::info!(event = "server.console_origin", origin = %origin);
            guard.with_console(origin)
        }
        None => guard,
    };
    let sign_in = match &config.identity {
        IdentityMode::Local => SignInSettings::Local,
        IdentityMode::Oidc { issuer, audience, .. } => SignInSettings::Oidc {
            issuer: issuer.clone(),
            client_id: audience.clone(),
        },
    };
    let secret_key = match config.secret_key {
        Some(key) => key,
        // Only `local` gets here: the config refuses `oidc` without a key.
        None => match database.instance_secret("secret_key").await {
            Ok(key) => key,
            Err(error) => {
                tracing::error!(event = "server.database_failed", error = %error);
                return ExitCode::FAILURE;
            }
        },
    };
    let outbound = match zupflow_outbound::SafeHttpClient::new() {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(event = "server.outbound_failed", %error);
            return ExitCode::FAILURE;
        }
    };
    // No model provider ships with Zupflow (decision 0014): runs are
    // answered only by one the deployment configures. Without one, every run
    // fails `model_unavailable`; the scripted model is for local work only.
    let provider: Arc<dyn ModelProvider> = match config.model_provider {
        ModelProviderMode::None => {
            tracing::warn!(
                event = "server.no_model_provider",
                "No model provider is configured (ZUPFLOW_MODEL_PROVIDER): every run fails model_unavailable"
            );
            Arc::new(zupflow_basic::no_model::NoModelProvider)
        }
        ModelProviderMode::Scripted => {
            let served: Vec<String> = match config.models.list_models().await {
                Ok(models) => models.into_iter().map(|model| model.id).collect(),
                Err(error) => {
                    tracing::error!(event = "server.models_failed", %error);
                    return ExitCode::FAILURE;
                }
            };
            tracing::warn!(
                event = "server.scripted_model",
                "Runs are answered by the scripted model (ZUPFLOW_MODEL_PROVIDER=scripted): for local work only"
            );
            Arc::new(zupflow_basic::scripted_model::ScriptedModel::serving(served))
        }
    };
    let state = AppState {
        identity,
        organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
        workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
        apps: Arc::new(zupflow_sqlite::SqliteAppStore::new(database.clone())),
        installations: Arc::new(zupflow_sqlite::SqliteInstallationStore::new(database.clone())),
        agents: Arc::new(zupflow_sqlite::SqliteAgentStore::new(database.clone())),
        policies: Arc::new(zupflow_sqlite::SqliteToolPolicyStore::new(database.clone())),
        runs: Arc::new(zupflow_sqlite::SqliteRunStore::new(database.clone())),
        provider,
        approvals: Arc::new(zupflow_basic::configured_policy::ConfiguredPolicy::new(Arc::new(
            zupflow_sqlite::SqliteToolPolicyStore::new(database.clone()),
        ))),
        events: Arc::new(zupflow_basic::in_process_events::InProcessEventBus::default()),
        models: Arc::new(config.models.clone()),
        connect: Arc::new(zupflow_sqlite::SqliteConnectStore::new(database.clone())),
        jobs: Arc::new(zupflow_sqlite::SqliteJobQueue::new(database.clone())),
        tools: Arc::new(zupflow_sqlite::SqliteToolRegistry::new(database.clone())),
        cache: Arc::new(zupflow_basic::memory_cache::MemoryCache::default()),
        keys: Arc::new(zupflow_basic::local_keys::LocalKeyService::from_server_secret(
            &secret_key,
        )),
        outbound: Arc::new(outbound),
        public_url: config.public_url.clone(),
        sign_in,
        api_keys: Arc::new(SqliteApiKeyStore::new(database.clone())),
        api_key_hasher: Arc::new(ApiKeyHasher::new(secret_key)),
        idempotency: Arc::new(SqliteIdempotencyStore::new(database.clone())),
        fingerprinter: Arc::new(Fingerprinter::new(secret_key)),
        webhooks: Arc::new(zupflow_sqlite::SqliteWebhookStore::new(database.clone())),
    };

    let listener = match TcpListener::bind(config.listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(event = "server.bind_failed", address = %config.listen, %error);
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(event = "server.listening", address = %config.listen);
    // Background work, in the same process; stopped with the server.
    let worker = tokio::spawn(zupflow_server::discovery::run_worker(state.clone()));
    let runner = tokio::spawn(zupflow_server::run_engine::run_worker(state.clone()));
    let deliverer = tokio::spawn(zupflow_server::webhook_delivery::run_workers(state.clone()));
    let served = axum::serve(listener, zupflow_server::app(state, guard))
        .with_graceful_shutdown(shutdown())
        .await;
    worker.abort();
    runner.abort();
    deliverer.abort();
    if let Err(error) = served {
        tracing::error!(event = "server.failed", %error);
        return ExitCode::FAILURE;
    }
    tracing::info!(event = "server.stopped");
    ExitCode::SUCCESS
}

/// Finishes in-flight requests on Ctrl-C or SIGTERM.
async fn shutdown() {
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
