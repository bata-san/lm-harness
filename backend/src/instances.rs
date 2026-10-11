//! STUB — to be implemented. Inference servers, launch profiles, setup jobs, Ollama passthrough.
//! Routes: /api/instances*, /api/profiles*, /api/runtimes, /api/jobs*, /api/ollama/*
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use axum::Router;

use crate::config::SettingsStore;
use crate::mode::ModeProfile;
use crate::state::AppState;

pub struct InstanceManager {}

impl InstanceManager {
    pub fn load(settings: Arc<SettingsStore>) -> Arc<Self> {
        Arc::new(Self {})
    }
    /// Health/metrics loop; never returns.
    pub async fn monitor(self: Arc<Self>, state: AppState) {
        std::future::pending::<()>().await
    }
    pub async fn autostart(&self, state: &AppState) {}
    pub async fn stop_all(&self) {}
    /// Apply a new mode: re-pin/renice running servers live; if `restart_auto_tuned`, restart
    /// servers launched with auto_tune so their args match the new limits. Returns restarted ids.
    pub async fn apply_profile(&self, state: &AppState, profile: &ModeProfile, restart_auto_tuned: bool) -> Vec<String> {
        vec![]
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
}
