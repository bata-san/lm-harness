//! STUB — to be implemented. Download manager (segmented HF downloads, Ollama pulls).
//! Routes: GET /api/hf/search, GET /api/hf/files/{*repo}, /api/downloads...
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use axum::Router;
use tokio::sync::watch;

use crate::config::SettingsStore;
use crate::mode::ModeProfile;
use crate::state::AppState;

pub struct DownloadManager {}

impl DownloadManager {
    pub fn load(settings: Arc<SettingsStore>, mode_rx: watch::Receiver<ModeProfile>) -> Arc<Self> {
        Arc::new(Self {})
    }
    /// Scheduler loop; never returns.
    pub async fn run(self: Arc<Self>) {
        std::future::pending::<()>().await
    }
    /// Flush state to ~/.llm-harness/downloads.json.
    pub fn persist(&self) {}
}

pub fn router() -> Router<AppState> {
    Router::new()
}
