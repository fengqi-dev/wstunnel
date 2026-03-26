use anyhow::{Context, anyhow};
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use clap::Parser;
use serde::Deserialize;
use std::{collections::HashMap, fs, net::SocketAddr, sync::Arc};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Parser, Debug)]
struct Args {
    /// Bind address, e.g. 127.0.0.1:9000
    #[arg(long, default_value = "127.0.0.1:9000", env = "TUNNEL_SERVER_BIND")]
    bind: SocketAddr,

    /// YAML data file path
    ///
    /// Format:
    /// tunnels:
    ///   "<uuid>": { host: "example.com", port: 443 }
    #[arg(long, env = "TUNNEL_SERVER_DATA")]
    data: String,
}

#[derive(Clone)]
struct AppState {
    map: Arc<HashMap<Uuid, TunnelEntry>>,
}

#[derive(Debug, Clone, Deserialize)]
struct DataFile {
    #[serde(default)]
    tunnels: HashMap<Uuid, TunnelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct TunnelEntry {
    host: String,
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let map = load_data_file(&args.data)?;
    let state = AppState { map: Arc::new(map) };

    let app = Router::new()
        .route("/tunnels/{uuid}", get(get_tunnel))
        .with_state(state);

    info!("tunnel-server listening on {}", args.bind);
    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("cannot bind to {}", args.bind))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow!("server error: {e}"))?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn load_data_file(path: &str) -> anyhow::Result<HashMap<Uuid, TunnelEntry>> {
    let content = fs::read_to_string(path).with_context(|| format!("cannot read data file {path}"))?;
    let parsed: DataFile =
        serde_yaml::from_str(&content).with_context(|| format!("cannot parse YAML data file {path}"))?;
    Ok(parsed.tunnels)
}

async fn get_tunnel(State(state): State<AppState>, Path(uuid): Path<Uuid>) -> Response {
    let Some(entry) = state.map.get(&uuid) else {
        warn!("tunnel not found: {}", uuid);
        return StatusCode::NOT_FOUND.into_response();
    };

    // Match wstunnel server resolver expectation (YAML with host/port).
    let body = format!("host: {}\nport: {}\n", entry.host, entry.port);

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/yaml; charset=utf-8".parse().unwrap());
    (StatusCode::OK, headers, body).into_response()
}
