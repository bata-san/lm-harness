//! STUB — to be implemented. Latest-model catalog with hardware-aware one-click install/uninstall.
//! Routes: /api/catalog*
#![allow(dead_code, unused_variables)]

use axum::Router;

use crate::state::AppState;

/// Background loop: finishes pending installs (register profile / launch when the download completes).
pub async fn run(state: AppState) {
    std::future::pending::<()>().await
}

pub fn router() -> Router<AppState> {
    Router::new()
}
