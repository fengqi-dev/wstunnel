use futures::TryStreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client,
    runtime::{WatchStreamExt, watcher, watcher::Config},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::{AppState, TunnelEntry};

const ANN_TUNNEL_ID: &str = "wstunnel.io/tunnel-id";
const ANN_TUNNEL_PORT: &str = "wstunnel.io/tunnel-port";
const ANN_TUNNEL_RATE_LIMIT: &str = "wstunnel.io/tunnel-rate-limit";
const ANN_TUNNEL_RATE_LIMIT_UPLOAD: &str = "wstunnel.io/tunnel-rate-limit-upload";
const ANN_TUNNEL_RATE_LIMIT_DOWNLOAD: &str = "wstunnel.io/tunnel-rate-limit-download";
const DEFAULT_TUNNEL_PORT: u16 = 22;
const MAX_RATE_LIMIT_M: u16 = 10;

pub async fn watch_pods(client: Client, namespace: &str, state: AppState) {
    let api: Api<Pod> = if namespace.is_empty() {
        Api::all(client)
    } else {
        Api::namespaced(client, namespace)
    };

    let wc = Config::default();

    let stream = watcher(api, wc).default_backoff().applied_objects();
    futures::pin_mut!(stream);

    info!(
        "starting k8s pod watcher (namespace={})",
        if namespace.is_empty() { "<all>" } else { namespace },
    );

    loop {
        match stream.try_next().await {
            Ok(Some(pod)) => handle_pod_event(&state, &pod).await,
            Ok(None) => {
                warn!("pod watch stream ended");
                break;
            }
            Err(e) => {
                warn!("pod watcher error: {e}");
            }
        }
    }
}

async fn handle_pod_event(state: &AppState, pod: &Pod) {
    let pod_name = pod.metadata.name.as_deref().unwrap_or("<unknown>");

    let annotations = match pod.metadata.annotations.as_ref() {
        Some(a) => a,
        None => return,
    };

    let tunnel_id_str = match annotations.get(ANN_TUNNEL_ID) {
        Some(v) => v,
        None => return,
    };

    let tunnel_id: Uuid = match tunnel_id_str.parse() {
        Ok(id) => id,
        Err(e) => {
            warn!("pod {pod_name}: invalid tunnel-id annotation \"{tunnel_id_str}\": {e}");
            return;
        }
    };

    let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.as_deref()).unwrap_or("");

    let is_running = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .map(|p| p == "Running")
        .unwrap_or(false);

    let deletion_ts = pod.metadata.deletion_timestamp.is_some();

    if pod_ip.is_empty() || !is_running || deletion_ts {
        let mut map = state.map.write().await;
        if map.remove(&tunnel_id).is_some() {
            info!("removed tunnel {tunnel_id} (pod {pod_name}, running={is_running}, deleting={deletion_ts})");
        }
        return;
    }

    let port: u16 = annotations
        .get(ANN_TUNNEL_PORT)
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TUNNEL_PORT);

    // Backward-compatible: legacy single rate limit applies to both directions.
    fn parse_rate_limit(v: Option<&String>) -> Option<u16> {
        v.and_then(|s| s.trim().parse::<u16>().ok()).map(|n| n.min(MAX_RATE_LIMIT_M))
    }

    let legacy_rate_limit = parse_rate_limit(annotations.get(ANN_TUNNEL_RATE_LIMIT));
    let rate_limit_upload_m = parse_rate_limit(annotations.get(ANN_TUNNEL_RATE_LIMIT_UPLOAD))
        .or(legacy_rate_limit);
    let rate_limit_download_m = parse_rate_limit(annotations.get(ANN_TUNNEL_RATE_LIMIT_DOWNLOAD))
        .or(legacy_rate_limit);

    let entry = TunnelEntry {
        host: pod_ip.to_string(),
        port,
        rate_limit_upload_m,
        rate_limit_download_m,
    };

    debug!("upsert tunnel {tunnel_id} -> {}:{} (pod {pod_name})", entry.host, entry.port);
    state.map.write().await.insert(tunnel_id, entry);
}
