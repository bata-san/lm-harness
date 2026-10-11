//! STUB — to be implemented. Authentication for remote access (tunnels).
//! Routes: /api/auth/*
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;

use crate::state::AppState;

pub struct AuthManager {}

impl AuthManager {
    /// Load ~/.llm-harness/auth.json (+ persisted sessions).
    pub fn load() -> Arc<Self> {
        Arc::new(Self {})
    }
    /// True once an admin user/password exists.
    pub fn is_configured(&self) -> bool {
        false
    }
}

/// Axum middleware guarding every `/api/*` and `/v1/*` route (see module docs for rules).
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    next.run(req).await
}

/// Adds X-Frame-Options / nosniff / Referrer-Policy etc. to every response.
pub async fn security_headers(req: Request, next: Next) -> Response {
    next.run(req).await
}

/// CLI: `llm-harness auth reset --user <name>` (password from stdin). Works only locally.
pub fn cli_set_password(user: &str, password: &str) -> anyhow::Result<()> {
    anyhow::bail!("unimplemented")
}

/// CLI: `llm-harness auth disable-totp`.
pub fn cli_disable_totp() -> anyhow::Result<()> {
    anyhow::bail!("unimplemented")
}

pub fn router() -> Router<AppState> {
    Router::new()
}
