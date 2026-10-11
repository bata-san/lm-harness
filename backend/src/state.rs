use std::sync::Arc;

use tokio::sync::watch;

use crate::config::SettingsStore;
use crate::mode::ModeProfile;
use crate::{auth, downloads, instances, power, system, tunnel};

/// Shared application state handed to every axum handler (`State<AppState>`).
#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<SettingsStore>,
    pub telemetry: Arc<system::Telemetry>,
    pub downloads: Arc<downloads::DownloadManager>,
    pub instances: Arc<instances::InstanceManager>,
    pub power: Arc<power::PowerManager>,
    pub auth: Arc<auth::AuthManager>,
    pub tunnel: Arc<tunnel::TunnelManager>,
    /// Current resolved mode limits. `mode_rx.borrow().clone()` for a snapshot,
    /// `mode_rx.clone().changed().await` to react to switches.
    pub mode_rx: watch::Receiver<ModeProfile>,
}

impl AppState {
    pub fn profile(&self) -> ModeProfile {
        self.mode_rx.borrow().clone()
    }
}
