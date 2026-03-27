use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub map: Arc<RwLock<HashMap<Uuid, TunnelEntry>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            map: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TunnelEntry {
    pub host: String,
    pub port: u16,
}
