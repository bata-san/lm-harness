//! STUB — to be implemented. OpenAI-compatible reverse proxy `/v1/*` to running servers.
#![allow(dead_code, unused_variables)]

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
}
