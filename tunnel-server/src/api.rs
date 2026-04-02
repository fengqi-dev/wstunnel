use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use tracing::warn;
use uuid::Uuid;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/tunnels/{uuid}", get(get_tunnel))
}

async fn get_tunnel(State(state): State<AppState>, Path(uuid): Path<Uuid>) -> Response {
    let map = state.map.read().await;
    let Some(entry) = map.get(&uuid) else {
        warn!("tunnel not found: {}", uuid);
        return StatusCode::NOT_FOUND.into_response();
    };

    let mut body = format!("host: {}\nport: {}\n", entry.host, entry.port);
    if let Some(m) = entry.rate_limit_upload_m {
        body.push_str(&format!("rate_limit_upload_m: {}\n", m));
    }
    if let Some(m) = entry.rate_limit_download_m {
        body.push_str(&format!("rate_limit_download_m: {}\n", m));
    }

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/yaml; charset=utf-8".parse().unwrap());
    (StatusCode::OK, headers, body).into_response()
}
