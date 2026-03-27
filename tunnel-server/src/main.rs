mod api;
mod state;
mod watcher;

use anyhow::{Context, anyhow};
use clap::Parser;
use kube::Client;
use rustls::crypto::ring;
use std::net::SocketAddr;
use tracing::{debug, info};

use state::AppState;

#[derive(Parser, Debug)]
struct Args {
    /// Bind address, e.g. 127.0.0.1:9000
    #[arg(long, default_value = "127.0.0.1:9000", env = "TUNNEL_SERVER_BIND")]
    bind: SocketAddr,

    /// Kubernetes namespace to watch (empty = all namespaces)
    #[arg(long, default_value = "", env = "TUNNEL_SERVER_NAMESPACE")]
    namespace: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(_already_installed) = ring::default_provider().install_default() {
        debug!("rustls CryptoProvider already installed");
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let client = Client::try_default()
        .await
        .context("failed to create kubernetes client (is KUBECONFIG / in-cluster config available?)")?;

    let state = AppState::new();

    let watcher_state = state.clone();
    let ns = args.namespace.clone();
    let watcher_handle = tokio::spawn(async move {
        watcher::watch_pods(client, &ns, watcher_state).await;
    });

    let app = api::router().with_state(state);

    info!("tunnel-server listening on {}", args.bind);
    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("cannot bind to {}", args.bind))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow!("server error: {e}"))?;

    watcher_handle.abort();
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
