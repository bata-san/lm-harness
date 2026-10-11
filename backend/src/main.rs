mod affinity;
mod auth;
mod catalog;
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
mod proxy;
mod runtimes;
mod state;
mod system;
mod topology;
mod tunnel;
mod tuning;
mod types;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{middleware, Json, Router};
use clap::{Parser, Subcommand};
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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Local recovery for the admin account (run on the workstation itself).
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Set (or reset) the admin user's password. Password is read from stdin.
    SetPassword {
        #[arg(long, default_value = "admin")]
        user: String,
    },
    /// Turn off TOTP two-factor auth (e.g. lost phone).
    DisableTotp,
}

fn run_cli(cmd: Command) -> anyhow::Result<()> {
    match cmd {
        Command::Auth { action: AuthAction::SetPassword { user } } => {
            use std::io::BufRead;
            eprint!("新しいパスワード (12 文字以上): ");
            let mut pw = String::new();
            std::io::stdin().lock().read_line(&mut pw)?;
            auth::cli_set_password(&user, pw.trim_end_matches(['\r', '\n']))?;
            eprintln!("パスワードを設定しました。既存のセッションは無効化されます (ハーネスを再起動してください)。");
        }
        Command::Auth { action: AuthAction::DisableTotp } => {
            auth::cli_disable_totp()?;
            eprintln!("二段階認証を無効化しました。");
        }
    }
    Ok(())
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
    if let Some(cmd) = cli.command {
        return run_cli(cmd);
    }

    let settings = Arc::new(SettingsStore::load()?);
    let topo = topology::cpu_topology();
    let initial = mode::resolve(settings.get().mode.current, &settings.get(), topo);
    let (mode_tx, mode_rx) = watch::channel(initial);

    let state = AppState {
        telemetry: Arc::new(system::Telemetry::new()),
        downloads: downloads::DownloadManager::load(settings.clone(), mode_rx.clone()),
        instances: instances::InstanceManager::load(settings.clone()),
        power: power::PowerManager::new(settings.clone(), mode_tx),
        auth: auth::AuthManager::load(),
        tunnel: tunnel::TunnelManager::load(cli.port),
        settings,
        mode_rx,
    };

    tokio::spawn(state.downloads.clone().run());
    tokio::spawn(state.instances.clone().monitor(state.clone()));
    tokio::spawn(state.power.clone().run(state.clone()));
    tokio::spawn(state.tunnel.clone().run(state.clone()));
    tokio::spawn(catalog::run(state.clone()));
    state.instances.autostart(&state).await;

    let api = Router::new()
        .merge(auth::router())
        .merge(system::router())
        .merge(models::router())
        .merge(tuning::router())
        .merge(downloads::router())
        .merge(instances::router())
        .merge(power::router())
        .merge(tunnel::router())
        .merge(catalog::router())
        .merge(proxy::router())
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    let app = Router::new()
        // Unauthenticated liveness probe (used by the tunnel health check and systemd).
        .route("/api/health", get(|| async { Json(serde_json::json!({"ok": true, "version": env!("CARGO_PKG_VERSION")})) }))
        .merge(api)
        .fallback(static_handler)
        .layer(middleware::from_fn(auth::security_headers))
        .layer(CompressionLayer::new())
        .with_state(state.clone());

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("LLM Harness listening on http://{addr}  (mode: {:?})", state.profile().mode);

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shutting down: stopping servers");
    state.tunnel.stop().await;
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
