//! `taskboard`: serves the fake Taskboard app on loopback, for clicking
//! through Connect locally. Development only.
//!
//! ```text
//! taskboard [--port <port>] [--host <name>.example] [--certificate <path>] [--access-token-ttl <seconds>]
//! ```
//!
//! It listens on `127.0.0.1` (the port the system picks unless given), writes
//! its self-signed certificate to `--certificate` (PEM) for the backend to
//! trust, and prints `listening <port>`. A browser reaches it at
//! `https://taskboard.example:<port>` once that name points at `127.0.0.1`
//! (see DEVELOPMENT.md).

use std::process::ExitCode;
use std::time::Duration;

#[cfg(not(debug_assertions))]
compile_error!("zupflow-taskboard is for development only and is never built for release");

use zupflow_taskboard::{Options, start};

fn usage() -> ExitCode {
    eprintln!(
        "usage: taskboard [--port <port>] [--host <name>.example] [--certificate <path>] [--access-token-ttl <seconds>]"
    );
    ExitCode::FAILURE
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut options = Options::default();
    let mut certificate: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let Some(value) = args.next() else {
            return usage();
        };
        match flag.as_str() {
            "--port" => match value.parse() {
                Ok(port) => options.port = port,
                Err(_) => return usage(),
            },
            "--host" => options.host = value,
            "--certificate" => certificate = Some(value),
            "--access-token-ttl" => match value.parse::<u64>() {
                Ok(seconds) if seconds > 0 => options.access_token_ttl = Duration::from_secs(seconds),
                _ => return usage(),
            },
            _ => return usage(),
        }
    }
    let running = match start(options).await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("taskboard: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(path) = &certificate
        && let Err(error) = std::fs::write(path, &running.certificate_pem)
    {
        eprintln!("taskboard: can't write the certificate to {path}: {error}");
        return ExitCode::FAILURE;
    }
    #[allow(clippy::print_stdout)]
    {
        println!("listening {}", running.port);
        println!("mcp {}", running.taskboard.mcp_url());
    }
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
    ExitCode::SUCCESS
}
