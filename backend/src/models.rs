//! STUB — to be implemented. Local model inventory (GGUF files + HF safetensors dirs).
//! Routes: GET /api/models, DELETE /api/models/{*id}
#![allow(dead_code, unused_variables)]

use std::path::{Path, PathBuf};

use axum::Router;

use crate::state::AppState;
use crate::types::LocalModel;

pub fn scan(models_dir: &Path) -> Vec<LocalModel> {
    vec![]
}

/// Resolve a model id (relative path) inside models_dir, rejecting path traversal.
pub fn resolve(models_dir: &Path, id: &str) -> anyhow::Result<PathBuf> {
    anyhow::bail!("unimplemented")
}

pub fn delete(models_dir: &Path, id: &str) -> anyhow::Result<()> {
    anyhow::bail!("unimplemented")
}

pub fn router() -> Router<AppState> {
    Router::new()
}
