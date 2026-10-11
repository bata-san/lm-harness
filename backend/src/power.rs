//! STUB — to be implemented. Mode switching (full / shared), schedule, Windows bridge.
//! Routes: GET/PUT /api/mode, PUT /api/mode/settings
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use axum::Router;
use tokio::sync::watch;

use crate::config::SettingsStore;
use crate::mode::{Mode, ModeProfile};
use crate::state::AppState;

pub struct PowerManager {
    settings: Arc<SettingsStore>,
    mode_tx: watch::Sender<ModeProfile>,
}

impl PowerManager {
    pub fn new(settings: Arc<SettingsStore>, mode_tx: watch::Sender<ModeProfile>) -> Arc<Self> {
        Arc::new(Self { settings, mode_tx })
    }
    /// Startup apply + schedule loop; never returns.
    pub async fn run(self: Arc<Self>, state: AppState) {
        std::future::pending::<()>().await
    }
    pub async fn set_mode(&self, state: &AppState, mode: Mode, source: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
}
