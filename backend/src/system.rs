//! STUB — to be implemented. Host telemetry: GPU via NVML (fallback: nvidia-smi), CPU/mem/disk via sysinfo,
//! WSL health checks.
//! Routes: GET /api/system, GET /api/system/checks
#![allow(dead_code, unused_variables)]

use std::path::Path;

use axum::Router;

use crate::config::Settings;
use crate::state::AppState;
use crate::types::{Check, GpuInfo};

pub struct Telemetry {}

impl Telemetry {
    pub fn new() -> Self {
        Self {}
    }
    /// Current GPU readings (empty if no NVIDIA GPU / driver).
    pub fn gpus(&self) -> Vec<GpuInfo> {
        vec![]
    }
    /// JSON for GET /api/system (same shape as before: gpus, cpu, memory, swap, disk).
    pub fn snapshot(&self, models_dir: &Path) -> serde_json::Value {
        serde_json::json!({})
    }
    pub fn health_checks(&self, settings: &Settings) -> Vec<Check> {
        vec![]
    }
}

pub fn is_wsl() -> bool {
    false
}

pub fn router() -> Router<AppState> {
    Router::new()
}
