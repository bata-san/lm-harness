mod affinity;
mod config;
mod downloads;
mod error;
mod gguf;
mod hf;
mod instances;
mod mode;
mod models;
mod power;
mod procs;
mod runtimes;
mod state;
mod system;
mod topology;
mod tuning;
mod types;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use clap::Parser;
use rust_embed::RustEmbed;
use tokio::sync::watch;
use tower_http::compression::CompressionLayer;
use tracing_subscriber::EnvFilter;

use crate::config::SettingsStore;
use crate::state::AppState;

#[derive(Parser)]
#[command(name = "llm-harness", version, about = "Local LLM runtime harness")]
struct Cli {
    /// Bind address. 127.0.0.1 is reachable from Windows via WSL localhost forwarding;
    /// use 0.0.0.0 only to expose the (unauthenticated) API to the LAN.
    #[arg(long, env = "HOST", default_value = "127.0.0.1")]
    host: String,
    #[arg(long, env = "PORT", default_value_t = 8787)]
    port: u16,
}

#[derive(RustEmbed)]
#[folder = "../frontend/dist"]
struct Frontend;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn")))
        .init();
    let cli = Cli::parse();

    let settings = Arc::new(SettingsStore::load()?);
    let topo = topology::cpu_topology();
    let initial = mode::resolve(settings.get().mode.current, &settings.get(), topo);
    let (mode_tx, mode_rx) = watch::channel(initial);

    let state = AppState {
        telemetry: Arc::new(system::Telemetry::new()),
        downloads: downloads::DownloadManager::load(settings.clone(), mode_rx.clone()),
        instances: instances::InstanceManager::load(settings.clone()),
        power: power::PowerManager::new(settings.clone(), mode_tx),
        settings,
        mode_rx,
    };

    tokio::spawn(state.downloads.clone().run());
    tokio::spawn(state.instances.clone().monitor(state.clone()));
    tokio::spawn(state.power.clone().run(state.clone()));
    state.instances.autostart(&state).await;

    let app = Router::new()
        .merge(system::router())
        .merge(models::router())
        .merge(tuning::router())
        .merge(downloads::router())
        .merge(instances::router())
        .merge(power::router())
        .fallback(static_handler)
        .layer(CompressionLayer::new())
        .with_state(state.clone());

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("LLM Harness listening on http://{addr}  (mode: {:?})", state.profile().mode);

    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;

    tracing::info!("shutting down: stopping servers");
    state.instances.stop_all().await;
    state.downloads.persist();
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
}

/// Serves the embedded React build; unknown paths fall back to index.html (SPA routing).
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"detail": "Not Found"}))).into_response();
    }
    let (file, name) = match Frontend::get(path) {
        Some(f) if !path.is_empty() => (f, path),
        _ => match Frontend::get("index.html") {
            Some(f) => (f, "index.html"),
            None => return (StatusCode::NOT_FOUND, "frontend not built: run `npm run build` in frontend/").into_response(),
        },
    };
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let cache = if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    ([(header::CONTENT_TYPE, mime.as_ref()), (header::CACHE_CONTROL, cache)], file.data).into_response()
}
