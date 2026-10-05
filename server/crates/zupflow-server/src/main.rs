//! `zupflow`: runs the API server.

use std::process::ExitCode;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use zupflow_server::config::Config;

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

    let listener = match TcpListener::bind(config.listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(event = "server.bind_failed", address = %config.listen, %error);
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(event = "server.listening", address = %config.listen);
    if let Err(error) = axum::serve(listener, zupflow_server::app())
        .with_graceful_shutdown(shutdown())
        .await
    {
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
