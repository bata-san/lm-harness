//! STUB — to be implemented. llama.cpp launch-parameter recommendations.
//! Routes: POST /api/tune
#![allow(dead_code, unused_variables)]

use axum::Router;

use crate::mode::ModeProfile;
use crate::state::AppState;
use crate::types::{GgufInfo, GpuInfo, LocalModel, TuneResult};

pub fn kv_cache_bytes(info: &GgufInfo, ctx: u64, kv_type: &str, parallel: u32) -> u64 {
    0
}

pub fn recommend(model: &LocalModel, gpus: &[GpuInfo], profile: &ModeProfile, ctx: Option<u64>, kv_type: &str, parallel: u32) -> TuneResult {
    TuneResult::default()
}

/// Default args for vLLM under the given mode.
pub fn vllm_args(profile: &ModeProfile, gpus: &[GpuInfo]) -> Vec<String> {
    vec![]
}

/// Merge CLI flags: every flag present in `overrides` replaces the same flag in `base`.
pub fn merge_args(base: &[String], overrides: &[String]) -> Vec<String> {
    base.iter().chain(overrides).cloned().collect()
}

pub fn router() -> Router<AppState> {
    Router::new()
}
