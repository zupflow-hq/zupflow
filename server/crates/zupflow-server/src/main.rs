//! `zupflow`: runs the API server.

use std::process::ExitCode;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use zupflow_core::api_keys::ApiKeyHasher;
use zupflow_core::idempotency::Fingerprinter;
use zupflow_server::AppState;
use zupflow_server::bootstrap::{self, BootstrapError};
use zupflow_server::config::Config;
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
    let identity = match bootstrap::identity(&config) {
        Ok(identity) => identity,
        Err(error) => {
            tracing::error!(event = "server.identity_failed", %error);
            return ExitCode::FAILURE;
        }
    };
    let guard = bootstrap::guard(&config);
    let sign_in = bootstrap::sign_in(&config);
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
    let outbound = match bootstrap::outbound() {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(event = "server.outbound_failed", %error);
            return ExitCode::FAILURE;
        }
    };
    let provider = bootstrap::model_provider(&config);
    let model_catalog = bootstrap::model_catalog(&config);
    let runs = Arc::new(zupflow_sqlite::SqliteRunStore::new(database.clone()));
    let state = AppState {
        clock: Arc::new(zupflow_core::time::SystemClock),
        identity,
        organizations: Arc::new(SqliteOrganizationStore::new(database.clone())),
        workspaces: Arc::new(SqliteWorkspaceStore::new(database.clone())),
        apps: Arc::new(zupflow_sqlite::SqliteAppStore::new(database.clone())),
        installations: Arc::new(zupflow_sqlite::SqliteInstallationStore::new(database.clone())),
        agents: Arc::new(zupflow_sqlite::SqliteAgentStore::new(database.clone())),
        policies: Arc::new(zupflow_sqlite::SqliteToolPolicyStore::new(database.clone())),
        runs,
        provider,
        approvals: Arc::new(zupflow_basic::configured_policy::ConfiguredPolicy::new(Arc::new(
            zupflow_sqlite::SqliteToolPolicyStore::new(database.clone()),
        ))),
        events: Arc::new(zupflow_basic::in_process_events::InProcessEventBus::default()),
        metrics: Arc::new(zupflow_basic::in_process_metrics::InProcessMetrics::default()),
        model_store: Arc::new(zupflow_sqlite::SqliteModelStore::new(database.clone())),
        model_catalog,
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
        audit: Arc::new(zupflow_sqlite::SqliteAuditStore::new(database.clone())),
    };

    if let Err(error) = bootstrap::serve(&config, state, guard, bootstrap::shutdown_signal()).await {
        match error {
            BootstrapError::RunStoreNotDurable => tracing::error!(event = "server.run_store_not_durable"),
            BootstrapError::Bind { address, source } => {
                tracing::error!(event = "server.bind_failed", %address, error = %source);
            }
            error => tracing::error!(event = "server.failed", %error),
        }
        return ExitCode::FAILURE;
    }
    tracing::info!(event = "server.stopped");
    ExitCode::SUCCESS
}
