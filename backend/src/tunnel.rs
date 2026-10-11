//! STUB — to be implemented. Cloudflare Tunnel / ngrok supervision for remote access without
//! opening inbound ports.
//! Routes: /api/tunnel*
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub struct TunnelManager {}

impl TunnelManager {
    /// `local_port`: the harness HTTP port the tunnel forwards to (always 127.0.0.1).
    pub fn load(local_port: u16) -> Arc<Self> {
        Arc::new(Self {})
    }
    /// Autostart (if enabled) + supervision/health loop; never returns.
    pub async fn run(self: Arc<Self>, state: AppState) {
        std::future::pending::<()>().await
    }
    /// Stop the tunnel process (on shutdown).
    pub async fn stop(&self) {}
}

pub fn router() -> Router<AppState> {
    Router::new()
}
