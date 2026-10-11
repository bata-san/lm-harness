//! Remote access without inbound ports: Cloudflare Tunnel (quick / named-by-token) or ngrok.
//!
//! Design goals ("must never just stay broken"):
//!  * refuse to expose an unauthenticated panel (`start` is rejected until auth is configured);
//!  * install the helper binary on demand (atomic, verified) and retry installs with backoff;
//!  * a supervisor task owns the child process: exponential-backoff restarts, a 90 s start
//!    watchdog, and a public-internet health check (3 strikes -> restart);
//!  * a watchdog in `run()` re-spawns the supervisor itself should it ever die unexpectedly;
//!  * secrets only ever travel via environment variables and are masked in captured logs.
//!
//! Everything that decides something (command lines, log parsing, backoff, masking, config
//! merging) is a pure function with unit tests; the async plumbing wraps them.
//!
//! Routes: `/api/tunnel*`

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context};
use axum::extract::{Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::HARNESS_HOME;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Shown instead of any stored secret by `GET /api/tunnel`; sending it back means "unchanged".
pub const MASK: &str = "********";
const LOG_CAP: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const NGROK_API: &str = "http://127.0.0.1:4040/api/tunnels";
const MSG_AUTH_REQUIRED: &str =
    "先に認証を設定してください。認証なしでインターネットに公開するとパネルを誰でも操作できてしまうため、トンネルは開始できません。";
const TOKEN_HOSTNAME_HINT: &str = "Cloudflare ダッシュボードで設定したホスト名";

// ───────────────────────────── providers & binaries ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    CloudflareQuick,
    CloudflareToken,
    Ngrok,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "cloudflare_quick" => Some(Self::CloudflareQuick),
            "cloudflare_token" => Some(Self::CloudflareToken),
            "ngrok" => Some(Self::Ngrok),
            _ => None,
        }
    }
    pub fn bin_kind(self) -> BinKind {
        match self {
            Self::CloudflareQuick | Self::CloudflareToken => BinKind::Cloudflared,
            Self::Ngrok => BinKind::Ngrok,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinKind {
    Cloudflared,
    Ngrok,
}

impl BinKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cloudflared => "cloudflared",
            Self::Ngrok => "ngrok",
        }
    }
    fn version_args(self) -> &'static [&'static str] {
        match self {
            Self::Cloudflared => &["--version"],
            Self::Ngrok => &["version"],
        }
    }
    /// Accepts provider ids as well as plain binary names (`POST /api/tunnel/install`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "cloudflared" | "cloudflare" | "cloudflare_quick" | "cloudflare_token" => Some(Self::Cloudflared),
            "ngrok" => Some(Self::Ngrok),
            _ => None,
        }
    }
}

fn arch_tag(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// Official download location for the Linux build of `kind`.
pub fn download_url(kind: BinKind, arch: &str) -> anyhow::Result<String> {
    let tag = arch_tag(arch).ok_or_else(|| anyhow!("未対応の CPU アーキテクチャです: {arch}"))?;
    Ok(match kind {
        BinKind::Cloudflared => {
            format!("https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-{tag}")
        }
        BinKind::Ngrok => format!("https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-linux-{tag}.tgz"),
    })
}

fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// First executable called `name` on `path_var`.
pub fn find_in_path(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var).map(|d| d.join(name)).find(|p| is_executable_file(p))
}

/// PATH first, then `<home>/bin/`.
pub fn resolve_binary(kind: BinKind, home: &Path, path_var: &OsStr) -> Option<PathBuf> {
    find_in_path(kind.name(), path_var).or_else(|| Some(home.join("bin").join(kind.name())).filter(|p| is_executable_file(p)))
}

/// "cloudflared version 2026.9.0 (built ...)" / "ngrok version 3.18.4" -> "2026.9.0" / "3.18.4".
pub fn parse_version(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut words = line.split_whitespace();
    while let Some(w) = words.next() {
        if w.eq_ignore_ascii_case("version") {
            if let Some(v) = words.next() {
                return Some(v.trim_start_matches('v').to_string());
            }
        }
    }
    Some(line.chars().take(80).collect())
}

// ───────────────────────────── config ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TunnelConfig {
    pub provider: Provider,
    pub autostart: bool,
    pub cloudflare_token: String,
    /// Display URL for token mode, e.g. `https://llm.example.com`.
    pub cloudflare_hostname: String,
    pub ngrok_authtoken: String,
    /// Optional reserved/static ngrok domain.
    pub ngrok_domain: String,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            provider: Provider::CloudflareQuick,
            autostart: false,
            cloudflare_token: String::new(),
            cloudflare_hostname: String::new(),
            ngrok_authtoken: String::new(),
            ngrok_domain: String::new(),
        }
    }
}

fn mask_if_set(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        MASK.to_string()
    }
}

impl TunnelConfig {
    /// Copy that is safe to send to the browser.
    pub fn masked(&self) -> Self {
        Self {
            cloudflare_token: mask_if_set(&self.cloudflare_token),
            ngrok_authtoken: mask_if_set(&self.ngrok_authtoken),
            ..self.clone()
        }
    }

    /// Secrets that must never show up in logs.
    pub fn secrets(&self) -> Vec<String> {
        [&self.cloudflare_token, &self.ngrok_authtoken].into_iter().filter(|s| !s.is_empty()).cloned().collect()
    }

    /// Partial update. Unknown keys are ignored, `"********"` leaves a secret untouched.
    /// All-or-nothing: on error `self` is unchanged.
    pub fn apply_patch(&mut self, patch: &Value) -> Result<(), String> {
        let obj = patch.as_object().ok_or("設定は JSON オブジェクトで指定してください")?;
        let mut next = self.clone();
        for (key, val) in obj {
            match key.as_str() {
                "provider" => {
                    let s = val.as_str().ok_or("provider は文字列で指定してください")?;
                    next.provider = Provider::parse(s).ok_or_else(|| format!("不明なプロバイダです: {s}"))?;
                }
                "autostart" => next.autostart = val.as_bool().ok_or("autostart は true/false で指定してください")?,
                "cloudflare_token" | "cloudflare_hostname" | "ngrok_authtoken" | "ngrok_domain" => {
                    let s = match val {
                        Value::Null => "",
                        Value::String(s) => s.trim(),
                        _ => return Err(format!("{key} は文字列で指定してください")),
                    };
                    if s == MASK {
                        continue;
                    }
                    let slot = match key.as_str() {
                        "cloudflare_token" => &mut next.cloudflare_token,
                        "cloudflare_hostname" => &mut next.cloudflare_hostname,
                        "ngrok_authtoken" => &mut next.ngrok_authtoken,
                        _ => &mut next.ngrok_domain,
                    };
                    *slot = s.to_string();
                }
                _ => {}
            }
        }
        *self = next;
        Ok(())
    }

    /// Errors are user-facing (Japanese).
    pub fn validate(&self) -> Result<(), String> {
        match self.provider {
            Provider::CloudflareQuick => {}
            Provider::CloudflareToken => {
                if self.cloudflare_token.is_empty() {
                    return Err("Cloudflare のトンネルトークンが未設定です。Zero Trust ダッシュボードで作成したトークンを入力してください。".into());
                }
                if self.cloudflare_token.chars().any(char::is_whitespace) {
                    return Err("Cloudflare のトンネルトークンに空白が含まれています。コピーし直してください。".into());
                }
            }
            Provider::Ngrok => {
                if self.ngrok_authtoken.is_empty() {
                    return Err("ngrok の Authtoken が未設定です。ngrok ダッシュボードの Authtoken を入力してください。".into());
                }
                if self.ngrok_authtoken.chars().any(char::is_whitespace) {
                    return Err("ngrok の Authtoken に空白が含まれています。コピーし直してください。".into());
                }
                let d = normalize_domain(&self.ngrok_domain);
                if !d.is_empty() && !d.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
                    return Err(format!("ngrok のドメイン名が不正です: {}", self.ngrok_domain));
                }
            }
        }
        Ok(())
    }
}

/// Do these two configs require restarting a running tunnel? (`autostart` alone does not.)
pub fn needs_restart(old: &TunnelConfig, new: &TunnelConfig) -> bool {
    old.provider != new.provider
        || old.cloudflare_token != new.cloudflare_token
        || old.cloudflare_hostname != new.cloudflare_hostname
        || old.ngrok_authtoken != new.ngrok_authtoken
        || old.ngrok_domain != new.ngrok_domain
}

/// `llm.example.com` / `https://llm.example.com/` -> `https://llm.example.com`.
pub fn normalize_public_url(host: &str) -> Option<String> {
    let h = host.trim().trim_end_matches('/');
    if h.is_empty() {
        return None;
    }
    Some(if h.contains("://") { h.to_string() } else { format!("https://{h}") })
}

/// `https://my.ngrok.app/` -> `my.ngrok.app`.
pub fn normalize_domain(d: &str) -> String {
    let d = d.trim();
    let d = d.split_once("://").map(|(_, r)| r).unwrap_or(d);
    d.split('/').next().unwrap_or("").trim().to_string()
}

/// Atomic, owner-only write of the config file.
fn write_config_file(path: &Path, cfg: &TunnelConfig) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(cfg)?;
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_config_file(path: &Path) -> TunnelConfig {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!("tunnel.json を解析できません ({e}); 既定値を使用します");
            TunnelConfig::default()
        }),
        Err(_) => TunnelConfig::default(),
    }
}

// ───────────────────────────── command line & log parsing (pure) ─────────────────────────────

/// What to run. Secrets live in `env` only — never in `args` (visible via `ps`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub args: Vec<String>,
    pub env: Vec<(&'static str, String)>,
    pub env_remove: Vec<&'static str>,
}

pub fn build_command(cfg: &TunnelConfig, local_port: u16) -> CommandSpec {
    let origin = format!("http://127.0.0.1:{local_port}");
    match cfg.provider {
        Provider::CloudflareQuick => CommandSpec {
            args: ["tunnel", "--no-autoupdate", "--url", origin.as_str()].map(String::from).to_vec(),
            env: vec![],
            env_remove: vec!["TUNNEL_TOKEN", "NGROK_AUTHTOKEN"],
        },
        Provider::CloudflareToken => CommandSpec {
            args: ["tunnel", "--no-autoupdate", "run"].map(String::from).to_vec(),
            env: vec![("TUNNEL_TOKEN", cfg.cloudflare_token.clone())],
            env_remove: vec!["NGROK_AUTHTOKEN"],
        },
        Provider::Ngrok => {
            let mut args: Vec<String> =
                ["http", origin.as_str(), "--log", "stdout", "--log-format", "json"].map(String::from).to_vec();
            let domain = normalize_domain(&cfg.ngrok_domain);
            if !domain.is_empty() {
                args.push("--url".into());
                args.push(format!("https://{domain}"));
            }
            CommandSpec {
                args,
                env: vec![("NGROK_AUTHTOKEN", cfg.ngrok_authtoken.clone())],
                env_remove: vec!["TUNNEL_TOKEN"],
            }
        }
    }
}

/// What a single output line tells us.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineInfo {
    pub public_url: Option<String>,
    /// The tunnel is up (cloudflared: "Registered tunnel connection"; ngrok: "started tunnel").
    pub connected: bool,
    pub error: Option<String>,
}

static TRY_CF_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https://([a-z0-9-]+)\.trycloudflare\.com").expect("regex"));
static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new("\x1b\\[[0-9;]*[A-Za-z]").expect("regex"));
static CF_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"eyJ[A-Za-z0-9_\-+/=]{16,}").expect("regex"));
static NGROK_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9A-Za-z]{16,}_[0-9A-Za-z]{16,}\b").expect("regex"));
static KV_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(token["']?\s*[:=]\s*["']?)[^\s"',]{8,}"#).expect("regex"));

/// First `https://<name>.trycloudflare.com` in `line`. `api.trycloudflare.com` (the quick-tunnel
/// control API that shows up in error messages) is not a tunnel URL.
pub fn extract_quick_url(line: &str) -> Option<String> {
    TRY_CF_RE.captures_iter(line).find(|c| !matches!(&c[1], "api" | "www")).map(|c| c[0].to_string())
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

pub fn parse_cloudflared_line(line: &str) -> LineInfo {
    let mut info = LineInfo { public_url: extract_quick_url(line), ..Default::default() };
    if line.contains("Registered tunnel connection") {
        info.connected = true;
    }
    for marker in [" ERR ", " FTL "] {
        if let Some(i) = line.find(marker) {
            info.error = Some(clip(line[i + marker.len()..].trim(), 300));
            break;
        }
    }
    info
}

pub fn parse_ngrok_line(line: &str) -> LineInfo {
    let t = line.trim();
    let mut info = LineInfo::default();
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(t) {
        let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::trim);
        let msg = s("msg").unwrap_or("");
        let started = msg.contains("started tunnel") || s("obj") == Some("tunnels");
        if let Some(url) = s("url").filter(|u| u.starts_with("https://") && started) {
            info.public_url = Some(url.to_string());
            info.connected = true;
        }
        let err = s("err").filter(|e| !e.is_empty() && !matches!(*e, "<nil>" | "nil" | "null"));
        let lvl = s("lvl").unwrap_or("");
        if err.is_some() || matches!(lvl, "eror" | "error" | "crit") {
            info.error = Some(clip(&match err {
                Some(e) if !msg.is_empty() => format!("{msg}: {e}"),
                Some(e) => e.to_string(),
                None => msg.to_string(),
            }, 300));
        }
    } else if t.starts_with("ERROR:") || t.contains("ERR_NGROK_") {
        info.error = Some(clip(t, 300));
    }
    info
}

pub fn parse_line(provider: Provider, line: &str) -> LineInfo {
    match provider {
        Provider::CloudflareQuick | Provider::CloudflareToken => parse_cloudflared_line(line),
        Provider::Ngrok => parse_ngrok_line(line),
    }
}

/// `GET http://127.0.0.1:4040/api/tunnels` -> public https URL of a tunnel that points at `port`.
pub fn parse_ngrok_api(body: &Value, port: u16) -> Option<String> {
    let want = format!(":{port}");
    body.get("tunnels")?
        .as_array()?
        .iter()
        .filter(|t| {
            t.pointer("/config/addr").and_then(Value::as_str).map(|a| a.contains(&want)).unwrap_or(true)
        })
        .filter_map(|t| t.get("public_url").and_then(Value::as_str))
        .find(|u| u.starts_with("https://"))
        .map(str::to_string)
}

/// Remove ANSI escapes and anything that looks like a credential.
pub fn mask_secrets(line: &str, secrets: &[String]) -> String {
    let mut s = ANSI_RE.replace_all(line, "").into_owned();
    for sec in secrets {
        if sec.len() >= 6 {
            s = s.replace(sec.as_str(), MASK);
        }
    }
    let s = CF_TOKEN_RE.replace_all(&s, MASK);
    let s = KV_TOKEN_RE.replace_all(&s, format!("${{1}}{MASK}").as_str());
    NGROK_TOKEN_RE.replace_all(&s, MASK).into_owned()
}

// ───────────────────────────── timing, backoff, health (pure) ─────────────────────────────

#[derive(Clone, Debug)]
pub struct Timings {
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// Continuous connected time after which backoff resets and the old error is forgiven.
    pub stable_reset: Duration,
    pub health_interval: Duration,
    /// Quick-tunnel hostnames need a moment to resolve; don't probe right after connecting.
    pub health_grace: Duration,
    pub health_timeout: Duration,
    pub health_fail_limit: u32,
    pub start_timeout: Duration,
    pub kill_grace: Duration,
    pub ngrok_poll_after: Duration,
    pub watchdog_interval: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            backoff_base: Duration::from_secs(2),
            backoff_cap: Duration::from_secs(60),
            stable_reset: Duration::from_secs(300),
            health_interval: Duration::from_secs(60),
            health_grace: Duration::from_secs(30),
            health_timeout: Duration::from_secs(15),
            health_fail_limit: 3,
            start_timeout: Duration::from_secs(90),
            kill_grace: Duration::from_secs(5),
            ngrok_poll_after: Duration::from_secs(8),
            watchdog_interval: Duration::from_secs(15),
        }
    }
}

/// 2 s, 4 s, 8 s, 16 s, 32 s, 60 s, 60 s, …
pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration) -> Duration {
    base.saturating_mul(1u32 << attempt.min(16)).min(cap)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthVerdict {
    Healthy,
    /// Consecutive failures so far (below the limit).
    Failing(u32),
    Restart,
}

#[derive(Debug, Clone)]
pub struct HealthTracker {
    fails: u32,
    limit: u32,
}

impl HealthTracker {
    pub fn new(limit: u32) -> Self {
        Self { fails: 0, limit: limit.max(1) }
    }
    pub fn record(&mut self, ok: bool) -> HealthVerdict {
        if ok {
            self.fails = 0;
            return HealthVerdict::Healthy;
        }
        self.fails += 1;
        if self.fails >= self.limit {
            HealthVerdict::Restart
        } else {
            HealthVerdict::Failing(self.fails)
        }
    }
}

/// The health endpoint answered 2xx with `{"ok": true, ...}`.
pub fn health_ok(status: u16, body: &str) -> bool {
    (200..300).contains(&status) && serde_json::from_str::<Value>(body).ok().and_then(|v| v.get("ok").and_then(Value::as_bool)) == Some(true)
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthOutcome {
    pub ok: bool,
    pub at: DateTime<Utc>,
    pub latency_ms: u64,
    pub error: Option<String>,
}

// ───────────────────────────── log ring buffer ─────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LogLine {
    pub seq: u64,
    pub text: String,
}

#[derive(Debug)]
pub struct LogBuf {
    lines: VecDeque<LogLine>,
    next_seq: u64,
    cap: usize,
}

impl LogBuf {
    pub fn new(cap: usize) -> Self {
        Self { lines: VecDeque::new(), next_seq: 1, cap: cap.max(1) }
    }
    pub fn push(&mut self, text: String) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.lines.len() >= self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(LogLine { seq, text });
        seq
    }
    /// Lines with `seq > since`, plus the newest seq overall (0 when nothing was ever logged).
    pub fn since(&self, since: u64) -> (Vec<LogLine>, u64) {
        (self.lines.iter().filter(|l| l.seq > since).cloned().collect(), self.next_seq - 1)
    }
}

// ───────────────────────────── runtime state ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelState {
    Stopped,
    Installing,
    Starting,
    Connected,
    Degraded,
    Restarting,
    Error,
}

#[derive(Clone, Debug)]
struct Runtime {
    state: TunnelState,
    public_url: Option<String>,
    connected_since: Option<DateTime<Utc>>,
    restarts: u64,
    last_error: Option<String>,
    last_health: Option<HealthOutcome>,
    detail: Option<String>,
    next_retry_at: Option<DateTime<Utc>>,
    installing: Option<BinKind>,
}

impl Runtime {
    fn new() -> Self {
        Self {
            state: TunnelState::Stopped,
            public_url: None,
            connected_since: None,
            restarts: 0,
            last_error: None,
            last_health: None,
            detail: None,
            next_retry_at: None,
            installing: None,
        }
    }
    fn clear_connection(&mut self) {
        self.public_url = None;
        self.connected_since = None;
        self.last_health = None;
        self.next_retry_at = None;
    }
}

type AuthFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// Test/production knobs.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub timings: Timings,
    /// Use exactly this binary (skips PATH lookup, verification and installation).
    pub cloudflared_bin: Option<PathBuf>,
    pub ngrok_bin: Option<PathBuf>,
    /// PATH to search; defaults to the process environment.
    pub path_var: Option<OsString>,
}

struct SupHandle {
    join: JoinHandle<()>,
    cancel: watch::Sender<bool>,
    reload: watch::Sender<u64>,
}

struct Ctl {
    cancel: watch::Receiver<bool>,
    reload: watch::Receiver<u64>,
}

enum Wake {
    Timeout,
    Cancel,
    Reload,
}

async fn wait_cancel(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_reload(rx: &mut watch::Receiver<u64>) {
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn sleep_until_opt(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

async fn wait_ctl(ctl: &mut Ctl, d: Duration) -> Wake {
    tokio::select! {
        _ = tokio::time::sleep(d) => Wake::Timeout,
        _ = wait_cancel(&mut ctl.cancel) => Wake::Cancel,
        _ = wait_reload(&mut ctl.reload) => Wake::Reload,
    }
}

enum Ev {
    Line(LineInfo),
    Health(HealthOutcome),
    Poll(Option<String>),
}

enum Act {
    Cancel,
    Reload,
    Exit(std::io::Result<std::process::ExitStatus>),
    Ev(Ev),
    HealthTick,
    StartTimeout,
    Stable,
    PollTick,
}

#[derive(Debug)]
enum SessionEnd {
    Cancelled,
    Reload,
    Exited,
    StartTimeout,
    HealthFailed,
    SpawnFailed,
}

struct SessionOutcome {
    end: SessionEnd,
    stable: bool,
}

struct ReaderGuard(Vec<JoinHandle<()>>);

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

/// SIGTERM the whole process group, escalate to SIGKILL after `grace`.
async fn terminate(child: &mut Child, grace: Duration) {
    let pgid = child.id().map(|p| p as i32);
    if let Some(p) = pgid {
        // SAFETY: plain signal delivery to the process group we created with process_group(0).
        unsafe { libc::killpg(p, libc::SIGTERM) };
    }
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        if let Some(p) = pgid {
            // SAFETY: as above.
            unsafe { libc::killpg(p, libc::SIGKILL) };
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

// ───────────────────────────── the manager ─────────────────────────────

pub struct TunnelManager {
    home: PathBuf,
    local_port: u16,
    opts: Options,
    cfg: Mutex<TunnelConfig>,
    rt: Mutex<Runtime>,
    logs: Mutex<LogBuf>,
    sup: Mutex<Option<SupHandle>>,
    /// The user (or autostart) wants the tunnel up; the watchdog revives a dead supervisor.
    desired: AtomicBool,
    /// The user pressed start/stop: boot-time autostart must not override that.
    touched: AtomicBool,
    auth: Mutex<Option<AuthFn>>,
    install_lock: tokio::sync::Mutex<()>,
    versions: Mutex<HashMap<PathBuf, (Option<SystemTime>, String)>>,
    http: OnceLock<reqwest::Client>,
}

impl TunnelManager {
    /// `local_port`: the harness HTTP port the tunnel forwards to (always 127.0.0.1).
    pub fn load(local_port: u16) -> Arc<Self> {
        Self::with_options(HARNESS_HOME.clone(), local_port, Options::default())
    }

    pub fn with_options(home: PathBuf, local_port: u16, opts: Options) -> Arc<Self> {
        let cfg = read_config_file(&home.join("tunnel.json"));
        Arc::new(Self {
            home,
            local_port,
            opts,
            cfg: Mutex::new(cfg),
            rt: Mutex::new(Runtime::new()),
            logs: Mutex::new(LogBuf::new(LOG_CAP)),
            sup: Mutex::new(None),
            desired: AtomicBool::new(false),
            touched: AtomicBool::new(false),
            auth: Mutex::new(None),
            install_lock: tokio::sync::Mutex::new(()),
            versions: Mutex::new(HashMap::new()),
            http: OnceLock::new(),
        })
    }

    /// Seam for "is an admin account configured?" (tests inject a closure).
    pub fn set_auth_check(&self, f: AuthFn) {
        *self.auth.lock() = Some(f);
    }

    pub fn bind_auth(&self, state: &AppState) {
        let auth = state.auth.clone();
        self.set_auth_check(Arc::new(move || auth.is_configured()));
    }

    fn auth_ok(&self) -> bool {
        let f = self.auth.lock().clone();
        f.map(|f| f()).unwrap_or(false)
    }

    // ── small helpers ──

    fn set_rt(&self, f: impl FnOnce(&mut Runtime)) {
        f(&mut self.rt.lock());
    }

    fn hlog(&self, msg: impl AsRef<str>) {
        let secrets = self.cfg.lock().secrets();
        let text = mask_secrets(msg.as_ref(), &secrets);
        tracing::info!(target: "tunnel", "{text}");
        self.logs.lock().push(format!("[harness] {text}"));
    }

    fn path_var(&self) -> OsString {
        self.opts.path_var.clone().or_else(|| std::env::var_os("PATH")).unwrap_or_default()
    }

    fn override_for(&self, kind: BinKind) -> Option<PathBuf> {
        match kind {
            BinKind::Cloudflared => self.opts.cloudflared_bin.clone(),
            BinKind::Ngrok => self.opts.ngrok_bin.clone(),
        }
    }

    fn resolve(&self, kind: BinKind) -> Option<PathBuf> {
        self.override_for(kind).or_else(|| resolve_binary(kind, &self.home, &self.path_var()))
    }

    fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(self.opts.timings.health_timeout)
                .redirect(reqwest::redirect::Policy::none())
                // Fresh connection every probe: a kept-alive socket could mask a dead tunnel.
                .pool_max_idle_per_host(0)
                .user_agent(concat!("llm-harness-tunnel/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default()
        })
    }

    // ── config ──

    pub fn config(&self) -> TunnelConfig {
        self.cfg.lock().clone()
    }

    /// Merge a partial config, persist it and (if it matters) restart a running tunnel.
    pub fn update_config(&self, patch: &Value) -> Result<(), ApiError> {
        let (old, new) = {
            let mut guard = self.cfg.lock();
            let old = guard.clone();
            let mut new = old.clone();
            new.apply_patch(patch).map_err(ApiError::bad_request)?;
            if new == old {
                return Ok(());
            }
            write_config_file(&self.home.join("tunnel.json"), &new).map_err(|e| ApiError::internal(format!("設定を保存できません: {e:#}")))?;
            *guard = new.clone();
            (old, new)
        };
        if needs_restart(&old, &new) {
            if let Some(h) = self.sup.lock().as_ref() {
                if !h.join.is_finished() {
                    self.hlog("設定が変更されたためトンネルを再起動します");
                    h.reload.send_modify(|g| *g += 1);
                }
            }
        }
        Ok(())
    }

    // ── public control ──

    /// Start supervision (idempotent). Refused unless auth is configured.
    pub fn start(self: &Arc<Self>) -> Result<(), ApiError> {
        self.touched.store(true, Ordering::SeqCst);
        self.start_inner(true)
    }

    fn start_inner(self: &Arc<Self>, fresh: bool) -> Result<(), ApiError> {
        if !self.auth_ok() {
            return Err(ApiError::bad_request(MSG_AUTH_REQUIRED));
        }
        let cfg = self.config();
        cfg.validate().map_err(ApiError::bad_request)?;
        let mut sup = self.sup.lock();
        if sup.as_ref().is_some_and(|h| !h.join.is_finished()) {
            return Ok(());
        }
        self.set_rt(|r| {
            if fresh {
                r.restarts = 0;
                r.last_error = None;
            }
            r.clear_connection();
            r.state = TunnelState::Starting;
            r.detail = Some("準備中…".into());
        });
        *sup = Some(self.spawn_supervisor());
        self.desired.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn spawn_supervisor(self: &Arc<Self>) -> SupHandle {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (reload_tx, reload_rx) = watch::channel(0u64);
        let me = self.clone();
        let join = tokio::spawn(async move { me.supervise(Ctl { cancel: cancel_rx, reload: reload_rx }).await });
        SupHandle { join, cancel: cancel_tx, reload: reload_tx }
    }

    /// Stop the tunnel process (manual stop or shutdown). Cancels supervision.
    pub async fn stop(&self) {
        self.touched.store(true, Ordering::SeqCst);
        self.desired.store(false, Ordering::SeqCst);
        let handle = self.sup.lock().take();
        if let Some(h) = handle {
            let _ = h.cancel.send(true);
            let mut join = h.join;
            if tokio::time::timeout(self.opts.timings.kill_grace + Duration::from_secs(5), &mut join).await.is_err() {
                join.abort();
            }
        }
        self.set_rt(|r| {
            r.state = TunnelState::Stopped;
            r.detail = None;
            r.clear_connection();
        });
        self.hlog("トンネルを停止しました");
    }

    /// Download + verify + atomically install `kind` in the background (`POST /install`).
    pub fn install_async(self: &Arc<Self>, kind: BinKind) {
        {
            let mut rt = self.rt.lock();
            if rt.installing.is_some() {
                return;
            }
            rt.installing = Some(kind);
        }
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.install(kind, true).await {
                let msg = format!("{} のインストールに失敗しました: {e:#}", kind.name());
                me.hlog(&msg);
                me.set_rt(|r| r.last_error = Some(msg));
            }
        });
    }

    /// Autostart + supervision watchdog; never returns.
    pub async fn run(self: Arc<Self>, state: AppState) {
        self.bind_auth(&state);
        let mut autostart_pending = self.config().autostart;
        let mut warned = false;
        loop {
            if autostart_pending {
                if self.touched.load(Ordering::SeqCst) {
                    autostart_pending = false;
                } else if self.auth_ok() {
                    autostart_pending = false;
                    match self.start_inner(true) {
                        Ok(()) => tracing::info!("トンネルを自動起動しました"),
                        Err(e) => tracing::warn!("トンネルの自動起動に失敗しました: {}", e.1),
                    }
                } else if !warned {
                    warned = true;
                    tracing::warn!("トンネルの自動起動は認証が未設定のため見送りました (認証を設定すると自動で開始します)");
                }
            }
            self.watchdog();
            tokio::time::sleep(self.opts.timings.watchdog_interval).await;
        }
    }

    /// If the supervisor task died although the tunnel should be up, bring it back.
    fn watchdog(self: &Arc<Self>) {
        if !self.desired.load(Ordering::SeqCst) {
            return;
        }
        let mut sup = self.sup.lock();
        if sup.as_ref().is_some_and(|h| !h.join.is_finished()) {
            return;
        }
        drop(sup.take());
        drop(sup);
        tracing::warn!("トンネル監視タスクが停止していたため再起動します");
        self.set_rt(|r| {
            r.restarts += 1;
            r.last_error = Some("監視タスクが異常終了したため再起動しました".into());
        });
        if self.start_inner(false).is_err() {
            self.desired.store(false, Ordering::SeqCst);
        }
    }

    // ── status / logs ──

    pub fn logs_since(&self, since: u64) -> Value {
        let (lines, last) = self.logs.lock().since(since);
        json!({ "lines": lines, "last": last })
    }

    pub async fn status_json(&self) -> Value {
        let rt = self.rt.lock().clone();
        let cfg = self.config();
        let state = if rt.installing.is_some() && matches!(rt.state, TunnelState::Stopped | TunnelState::Error) {
            TunnelState::Installing
        } else {
            rt.state
        };
        let hint = (cfg.provider == Provider::CloudflareToken && rt.public_url.is_none()).then_some(TOKEN_HOSTNAME_HINT);
        json!({
            "state": state,
            "provider": cfg.provider,
            "public_url": rt.public_url,
            "public_url_hint": hint,
            "connected_since": rt.connected_since,
            "restarts": rt.restarts,
            "last_error": rt.last_error,
            "last_health": rt.last_health,
            "auth_configured": self.auth_ok(),
            "detail": rt.detail,
            "next_retry_at": rt.next_retry_at,
            "installing": rt.installing.map(BinKind::name),
            "binaries": {
                "cloudflared": self.bin_info(BinKind::Cloudflared).await,
                "ngrok": self.bin_info(BinKind::Ngrok).await,
            },
            "config": cfg.masked(),
        })
    }

    async fn bin_info(&self, kind: BinKind) -> Value {
        match self.resolve(kind) {
            Some(path) => {
                let version = self.version_of(&path, kind).await.ok();
                json!({ "path": path, "version": version })
            }
            None => Value::Null,
        }
    }

    // ── binaries ──

    /// `<bin> --version`, cached per (path, mtime).
    async fn version_of(&self, path: &Path, kind: BinKind) -> anyhow::Result<String> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if let Some((m, v)) = self.versions.lock().get(path) {
            if *m == mtime {
                return Ok(v.clone());
            }
        }
        let out = tokio::time::timeout(
            Duration::from_secs(15),
            Command::new(path).args(kind.version_args()).stdin(Stdio::null()).kill_on_drop(true).output(),
        )
        .await
        .map_err(|_| anyhow!("バージョン確認がタイムアウトしました"))?
        .with_context(|| format!("{} を実行できません", path.display()))?;
        if !out.status.success() {
            bail!("バージョン確認が失敗しました ({})", out.status);
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        let v = parse_version(&text).ok_or_else(|| anyhow!("バージョン出力が空です"))?;
        self.versions.lock().insert(path.to_path_buf(), (mtime, v.clone()));
        Ok(v)
    }

    /// A usable binary: override, PATH, `<home>/bin`, or install it now.
    async fn ensure_binary(&self, kind: BinKind) -> anyhow::Result<PathBuf> {
        if let Some(p) = self.override_for(kind) {
            return Ok(p);
        }
        if let Some(p) = self.resolve(kind) {
            match self.version_of(&p, kind).await {
                Ok(_) => return Ok(p),
                Err(e) if p.starts_with(self.home.join("bin")) => {
                    self.hlog(format!("{} が壊れているため再インストールします: {e:#}", p.display()));
                    let _ = std::fs::remove_file(&p);
                }
                Err(e) => bail!("PATH 上の {} を実行できません: {e:#}", p.display()),
            }
        }
        self.set_rt(|r| {
            r.state = TunnelState::Installing;
            r.detail = Some(format!("{} をインストール中…", kind.name()));
        });
        self.install(kind, false).await
    }

    async fn install(&self, kind: BinKind, force: bool) -> anyhow::Result<PathBuf> {
        let _guard = self.install_lock.lock().await;
        self.set_rt(|r| r.installing = Some(kind));
        let res = self.install_locked(kind, force).await;
        self.set_rt(|r| r.installing = None);
        res
    }

    async fn install_locked(&self, kind: BinKind, force: bool) -> anyhow::Result<PathBuf> {
        let bin_dir = self.home.join("bin");
        let final_path = bin_dir.join(kind.name());
        if !force {
            // Someone else (a concurrent install) may have finished while we waited for the lock.
            if let Some(p) = self.resolve(kind) {
                if self.version_of(&p, kind).await.is_ok() {
                    return Ok(p);
                }
            }
        }
        std::fs::create_dir_all(&bin_dir).with_context(|| format!("create {}", bin_dir.display()))?;
        // Leftovers of an interrupted install.
        if let Ok(rd) = std::fs::read_dir(&bin_dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(".install-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let url = download_url(kind, std::env::consts::ARCH)?;
        let work = bin_dir.join(format!(".install-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&work)?;
        let res = self.install_in(kind, &url, &work, &final_path).await;
        let _ = std::fs::remove_dir_all(&work);
        let version = res?;
        self.hlog(format!("{} {} をインストールしました ({})", kind.name(), version, final_path.display()));
        Ok(final_path)
    }

    async fn install_in(&self, kind: BinKind, url: &str, work: &Path, final_path: &Path) -> anyhow::Result<String> {
        self.hlog(format!("{} をダウンロード中: {url}", kind.name()));
        let candidate = match kind {
            BinKind::Cloudflared => {
                let p = work.join("cloudflared");
                download_to(url, &p).await?;
                p
            }
            BinKind::Ngrok => {
                let tgz = work.join("ngrok.tgz");
                download_to(url, &tgz).await?;
                let out_dir = work.join("x");
                std::fs::create_dir_all(&out_dir)?;
                let out = tokio::time::timeout(
                    Duration::from_secs(120),
                    Command::new("tar").arg("-xzf").arg(&tgz).arg("-C").arg(&out_dir).stdin(Stdio::null()).kill_on_drop(true).output(),
                )
                .await
                .map_err(|_| anyhow!("tar がタイムアウトしました"))?
                .context("tar を実行できません")?;
                if !out.status.success() {
                    bail!("アーカイブの展開に失敗しました: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
                let p = out_dir.join("ngrok");
                if !p.is_file() {
                    bail!("アーカイブに ngrok が含まれていません");
                }
                p
            }
        };
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755))?;
        let version = self.version_of(&candidate, kind).await.context("ダウンロードしたバイナリの検証に失敗しました")?;
        // `work` lives inside the bin dir, so the rename stays on one filesystem (atomic).
        std::fs::rename(&candidate, final_path).context("バイナリの配置に失敗しました")?;
        self.versions.lock().remove(&candidate);
        Ok(version)
    }

    // ── supervision ──

    fn fail_terminal(&self, msg: &str) {
        tracing::warn!("トンネル: {msg}");
        self.logs.lock().push(format!("[harness] {msg}"));
        self.desired.store(false, Ordering::SeqCst);
        self.set_rt(|r| {
            r.state = TunnelState::Error;
            r.last_error = Some(msg.to_string());
            r.detail = None;
            r.clear_connection();
        });
    }

    async fn supervise(self: Arc<Self>, mut ctl: Ctl) {
        let t = self.opts.timings.clone();
        let mut attempt: u32 = 0;
        loop {
            if *ctl.cancel.borrow() {
                return;
            }
            ctl.reload.borrow_and_update();
            if !self.auth_ok() {
                self.fail_terminal(MSG_AUTH_REQUIRED);
                return;
            }
            let cfg = self.config();
            if let Err(e) = cfg.validate() {
                self.fail_terminal(&e);
                return;
            }
            let kind = cfg.provider.bin_kind();

            let bin = loop {
                let r = tokio::select! {
                    r = self.ensure_binary(kind) => Some(r),
                    _ = wait_cancel(&mut ctl.cancel) => None,
                };
                match r {
                    None => return,
                    Some(Ok(p)) => break p,
                    Some(Err(e)) => {
                        let msg = format!("{} の準備に失敗しました: {e:#}", kind.name());
                        let delay = backoff_delay(attempt, t.backoff_base, t.backoff_cap);
                        attempt = attempt.saturating_add(1);
                        self.hlog(format!("{msg} ({} 秒後に再試行)", delay.as_secs()));
                        self.set_rt(|r| {
                            r.state = TunnelState::Restarting;
                            r.last_error = Some(msg);
                            r.detail = Some(format!("{} 秒後に再試行します", delay.as_secs()));
                            r.next_retry_at = Some(Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default());
                        });
                        if matches!(wait_ctl(&mut ctl, delay).await, Wake::Cancel) {
                            return;
                        }
                    }
                }
            };

            let outcome = self.run_session(&cfg, &bin, &mut ctl).await;
            match outcome.end {
                SessionEnd::Cancelled => return,
                SessionEnd::Reload => {
                    attempt = 0;
                    continue;
                }
                _ => {}
            }
            if outcome.stable {
                attempt = 0;
            }
            let delay = backoff_delay(attempt, t.backoff_base, t.backoff_cap);
            attempt = attempt.saturating_add(1);
            self.hlog(format!("トンネルが停止しました ({:?})。{} 秒後に再起動します", outcome.end, delay.as_secs()));
            self.set_rt(|r| {
                r.restarts += 1;
                r.state = TunnelState::Restarting;
                r.clear_connection();
                r.detail = Some(format!("{} 秒後に再起動します", delay.as_secs()));
                r.next_retry_at = Some(Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default());
            });
            match wait_ctl(&mut ctl, delay).await {
                Wake::Cancel => return,
                Wake::Reload => attempt = 0,
                Wake::Timeout => {}
            }
        }
    }

    fn spawn_reader<R>(self: &Arc<Self>, r: R, provider: Provider, secrets: Arc<Vec<String>>, tx: mpsc::UnboundedSender<Ev>) -> JoinHandle<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let me = self.clone();
        tokio::spawn(async move {
            let mut rd = BufReader::new(r);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match rd.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let raw = String::from_utf8_lossy(&buf);
                let line = raw.trim_end_matches(['\r', '\n']);
                if line.trim().is_empty() {
                    continue;
                }
                let masked = mask_secrets(line, &secrets);
                let info = parse_line(provider, &masked);
                me.logs.lock().push(clip(&masked, MAX_LINE_CHARS));
                let _ = tx.send(Ev::Line(info));
            }
        })
    }

    /// One process lifetime: spawn, watch, and return why it ended.
    async fn run_session(self: &Arc<Self>, cfg: &TunnelConfig, bin: &Path, ctl: &mut Ctl) -> SessionOutcome {
        let t = self.opts.timings.clone();
        let provider = cfg.provider;
        let spec = build_command(cfg, self.local_port);
        let mut cmd = Command::new(bin);
        cmd.args(&spec.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).process_group(0);
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        self.set_rt(|r| {
            r.clear_connection();
            r.state = TunnelState::Starting;
            r.detail = Some("トンネルを起動中…".into());
        });
        // `args` never contain secrets (they go through the environment).
        self.hlog(format!("起動: {} {}", bin.display(), spec.args.join(" ")));
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("プロセスを起動できません ({}): {e}", bin.display());
                self.hlog(&msg);
                self.set_rt(|r| r.last_error = Some(msg));
                return SessionOutcome { end: SessionEnd::SpawnFailed, stable: false };
            }
        };

        let secrets = Arc::new(cfg.secrets());
        let (tx, mut rx) = mpsc::unbounded_channel::<Ev>();
        let mut readers = ReaderGuard(vec![]);
        if let Some(o) = child.stdout.take() {
            readers.0.push(self.spawn_reader(o, provider, secrets.clone(), tx.clone()));
        }
        if let Some(e) = child.stderr.take() {
            readers.0.push(self.spawn_reader(e, provider, secrets.clone(), tx.clone()));
        }

        let started = Instant::now();
        let mut start_deadline = Some(started + t.start_timeout);
        let mut next_poll = (provider == Provider::Ngrok).then(|| started + t.ngrok_poll_after);
        let mut next_health: Option<Instant> = None;
        let mut stable_at: Option<Instant> = None;
        let mut connected_at: Option<Instant> = None;
        let mut health_url: Option<String> = None;
        let mut tracker = HealthTracker::new(t.health_fail_limit);
        let mut url: Option<String> = None;
        let mut registered = false;
        let mut session_error: Option<String> = None;

        let end = loop {
            let act = tokio::select! {
                _ = wait_cancel(&mut ctl.cancel) => Act::Cancel,
                _ = wait_reload(&mut ctl.reload) => Act::Reload,
                st = child.wait() => Act::Exit(st),
                Some(ev) = rx.recv() => Act::Ev(ev),
                _ = sleep_until_opt(next_health) => Act::HealthTick,
                _ = sleep_until_opt(start_deadline) => Act::StartTimeout,
                _ = sleep_until_opt(stable_at) => Act::Stable,
                _ = sleep_until_opt(next_poll) => Act::PollTick,
            };
            match act {
                Act::Cancel => {
                    terminate(&mut child, t.kill_grace).await;
                    break SessionEnd::Cancelled;
                }
                Act::Reload => {
                    terminate(&mut child, t.kill_grace).await;
                    break SessionEnd::Reload;
                }
                Act::Exit(st) => {
                    // Let the readers flush the final lines (they often carry the reason).
                    for h in readers.0.iter_mut() {
                        let _ = tokio::time::timeout(Duration::from_millis(500), h).await;
                    }
                    while let Ok(ev) = rx.try_recv() {
                        if let Ev::Line(LineInfo { error: Some(e), .. }) = ev {
                            session_error = Some(e);
                        }
                    }
                    let code = match st {
                        Ok(s) => s.to_string(),
                        Err(e) => format!("待機に失敗: {e}"),
                    };
                    let msg = match &session_error {
                        Some(e) => format!("プロセスが終了しました ({code}): {e}"),
                        None => format!("プロセスが終了しました ({code})"),
                    };
                    self.set_rt(|r| r.last_error = Some(msg));
                    break SessionEnd::Exited;
                }
                Act::StartTimeout => {
                    let msg = format!("起動から {} 秒以内に接続できなかったため再起動します", t.start_timeout.as_secs());
                    self.hlog(&msg);
                    self.set_rt(|r| r.last_error = Some(msg));
                    terminate(&mut child, t.kill_grace).await;
                    break SessionEnd::StartTimeout;
                }
                Act::Stable => {
                    stable_at = None;
                    // Long, healthy run: forgive the past (the supervisor also resets backoff).
                    self.set_rt(|r| r.last_error = None);
                }
                Act::PollTick => {
                    next_poll = Some(Instant::now() + Duration::from_secs(3));
                    let (me, tx2) = (self.clone(), tx.clone());
                    tokio::spawn(async move {
                        let _ = tx2.send(Ev::Poll(me.poll_ngrok_api().await));
                    });
                }
                Act::HealthTick => {
                    next_health = None;
                    if let Some(u) = health_url.clone() {
                        let (me, tx2) = (self.clone(), tx.clone());
                        tokio::spawn(async move {
                            let _ = tx2.send(Ev::Health(me.check_health(&u).await));
                        });
                    }
                }
                Act::Ev(Ev::Health(h)) => {
                    if connected_at.is_none() {
                        continue;
                    }
                    let (ok, err) = (h.ok, h.error.clone());
                    self.set_rt(|r| r.last_health = Some(h));
                    match tracker.record(ok) {
                        HealthVerdict::Healthy => self.set_rt(|r| {
                            r.state = TunnelState::Connected;
                            r.detail = None;
                        }),
                        HealthVerdict::Failing(n) => {
                            let msg = format!("ヘルスチェックに失敗しました ({n}/{}): {}", t.health_fail_limit, err.unwrap_or_default());
                            self.hlog(&msg);
                            self.set_rt(|r| {
                                r.state = TunnelState::Degraded;
                                r.detail = Some(msg);
                            });
                        }
                        HealthVerdict::Restart => {
                            let msg = format!(
                                "ヘルスチェックに {} 回連続で失敗したため再起動します: {}",
                                t.health_fail_limit,
                                err.unwrap_or_default()
                            );
                            self.hlog(&msg);
                            self.set_rt(|r| {
                                r.state = TunnelState::Degraded;
                                r.last_error = Some(msg);
                            });
                            terminate(&mut child, t.kill_grace).await;
                            break SessionEnd::HealthFailed;
                        }
                    }
                    next_health = Some(Instant::now() + t.health_interval);
                }
                Act::Ev(ev @ (Ev::Line(_) | Ev::Poll(_))) => {
                    let info = match ev {
                        Ev::Line(i) => i,
                        Ev::Poll(Some(u)) => LineInfo { public_url: Some(u), connected: true, error: None },
                        _ => continue,
                    };
                    if let Some(e) = info.error {
                        session_error = Some(e.clone());
                        self.set_rt(|r| r.last_error = Some(e));
                    }
                    if url.is_none() {
                        url = info.public_url;
                    }
                    registered |= info.connected;
                    if connected_at.is_none() && registered && (url.is_some() || provider == Provider::CloudflareToken) {
                        let now = Instant::now();
                        connected_at = Some(now);
                        start_deadline = None;
                        next_poll = None;
                        stable_at = Some(now + t.stable_reset);
                        let public = if provider == Provider::CloudflareToken { normalize_public_url(&cfg.cloudflare_hostname) } else { url.clone() };
                        next_health = public.as_ref().map(|_| now + t.health_grace);
                        health_url = public.clone();
                        self.hlog(format!("接続しました: {}", public.as_deref().unwrap_or(TOKEN_HOSTNAME_HINT)));
                        self.set_rt(|r| {
                            r.state = TunnelState::Connected;
                            r.public_url = public;
                            r.connected_since = Some(Utc::now());
                            r.detail = None;
                            r.next_retry_at = None;
                        });
                    }
                }
            }
        };
        let stable = connected_at.is_some_and(|c| c.elapsed() >= t.stable_reset);
        SessionOutcome { end, stable }
    }

    async fn check_health(&self, public_url: &str) -> HealthOutcome {
        let url = format!("{}/api/health", public_url.trim_end_matches('/'));
        let started = Instant::now();
        let (ok, error) = match self.http().get(&url).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                if health_ok(status, &body) {
                    (true, None)
                } else {
                    (false, Some(format!("HTTP {status}")))
                }
            }
            Err(e) => (false, Some(clip(&e.to_string(), 200))),
        };
        HealthOutcome { ok, at: Utc::now(), latency_ms: started.elapsed().as_millis() as u64, error }
    }

    /// ngrok fallback when its JSON log did not reveal the URL.
    async fn poll_ngrok_api(&self) -> Option<String> {
        let resp = self.http().get(NGROK_API).send().await.ok()?;
        let body: Value = resp.json().await.ok()?;
        parse_ngrok_api(&body, self.local_port)
    }
}

async fn download_to(url: &str, dest: &Path) -> anyhow::Result<u64> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .user_agent(concat!("llm-harness-tunnel/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(url).send().await.with_context(|| format!("接続できません: {url}"))?;
    let resp = resp.error_for_status().with_context(|| format!("ダウンロードに失敗しました: {url}"))?;
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("ダウンロードが中断されました")?;
        total += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    if total < 1_000_000 {
        bail!("ダウンロードしたファイルが小さすぎます ({total} bytes)");
    }
    Ok(total)
}

// ───────────────────────────── routes ─────────────────────────────

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/tunnel", get(h_status))
        .route("/api/tunnel/config", put(h_config))
        .route("/api/tunnel/start", post(h_start))
        .route("/api/tunnel/stop", post(h_stop))
        .route("/api/tunnel/install", post(h_install))
        .route("/api/tunnel/logs", get(h_logs))
}

/// Status object with `"ok": true` merged in (returned by every mutating endpoint).
async fn status_ok(m: &TunnelManager) -> Value {
    let mut v = m.status_json().await;
    v["ok"] = Value::Bool(true);
    v
}

async fn h_status(State(s): State<AppState>) -> ApiResult<Value> {
    s.tunnel.bind_auth(&s);
    Ok(Json(s.tunnel.status_json().await))
}

async fn h_config(State(s): State<AppState>, Json(patch): Json<Value>) -> ApiResult<Value> {
    s.tunnel.bind_auth(&s);
    s.tunnel.update_config(&patch)?;
    Ok(Json(status_ok(&s.tunnel).await))
}

async fn h_start(State(s): State<AppState>) -> ApiResult<Value> {
    s.tunnel.bind_auth(&s);
    s.tunnel.start()?;
    Ok(Json(status_ok(&s.tunnel).await))
}

async fn h_stop(State(s): State<AppState>) -> ApiResult<Value> {
    s.tunnel.stop().await;
    Ok(Json(status_ok(&s.tunnel).await))
}

#[derive(Deserialize)]
struct InstallReq {
    provider: String,
}

async fn h_install(State(s): State<AppState>, Json(req): Json<InstallReq>) -> ApiResult<Value> {
    let kind = BinKind::parse(&req.provider).ok_or_else(|| ApiError::bad_request(format!("不明なプロバイダです: {}", req.provider)))?;
    s.tunnel.install_async(kind);
    Ok(Json(status_ok(&s.tunnel).await))
}

#[derive(Deserialize)]
struct LogsQuery {
    since: Option<u64>,
}

async fn h_logs(State(s): State<AppState>, Query(q): Query<LogsQuery>) -> ApiResult<Value> {
    Ok(Json(s.tunnel.logs_since(q.since.unwrap_or(0))))
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> String {
        x.to_string()
    }

    // ── parsing ──

    #[test]
    fn quick_url_from_boxed_cloudflared_line() {
        let l = "2026-10-11T03:00:00Z INF |  https://quiet-river-1234.trycloudflare.com                                  |";
        assert_eq!(extract_quick_url(l).as_deref(), Some("https://quiet-river-1234.trycloudflare.com"));
        let info = parse_cloudflared_line(l);
        assert_eq!(info.public_url.as_deref(), Some("https://quiet-river-1234.trycloudflare.com"));
        assert!(!info.connected);
        assert_eq!(info.error, None);
    }

    #[test]
    fn quick_url_ignores_api_host_and_noise() {
        let l = r#"2026-10-11T03:00:00Z ERR failed to request quick Tunnel: Post "https://api.trycloudflare.com/tunnel": dial tcp: lookup api.trycloudflare.com"#;
        assert_eq!(extract_quick_url(l), None);
        let info = parse_cloudflared_line(l);
        assert_eq!(info.public_url, None);
        assert!(info.error.unwrap().starts_with("failed to request quick Tunnel"));
        assert_eq!(extract_quick_url("INF Your quick Tunnel has been created! Visit it at (it may take some time to be reachable):"), None);
        assert_eq!(
            extract_quick_url("see https://api.trycloudflare.com and https://calm-fox-9.trycloudflare.com ok").as_deref(),
            Some("https://calm-fox-9.trycloudflare.com")
        );
    }

    #[test]
    fn cloudflared_registered_detection() {
        let l = "2026-10-11T03:00:04Z INF Registered tunnel connection connIndex=0 connection=abc event=0 ip=198.41.200.13 location=nrt01 protocol=quic";
        let i = parse_cloudflared_line(l);
        assert!(i.connected);
        assert_eq!(i.error, None);
        assert!(!parse_cloudflared_line("2026-10-11T03:00:04Z INF Starting tunnel tunnelID=abc").connected);
    }

    #[test]
    fn ngrok_json_lines() {
        let l = r#"{"lvl":"info","msg":"started tunnel","obj":"tunnels","name":"command_line","addr":"http://127.0.0.1:8787","url":"https://abc.ngrok-free.app"}"#;
        let i = parse_ngrok_line(l);
        assert_eq!(i.public_url.as_deref(), Some("https://abc.ngrok-free.app"));
        assert!(i.connected);
        assert_eq!(i.error, None);
        // the literal example from the spec
        let i = parse_ngrok_line(r#"{"lvl":"info","msg":"started tunnel","url":"https://abc.ngrok-free.app"}"#);
        assert!(i.connected);

        let e = parse_ngrok_line(r#"{"lvl":"eror","msg":"failed to auth","err":"authentication failed: bad token"}"#);
        assert_eq!(e.error.as_deref(), Some("failed to auth: authentication failed: bad token"));
        assert!(!e.connected);
        let e = parse_ngrok_line(r#"{"lvl":"eror","msg":"session closed"}"#);
        assert_eq!(e.error.as_deref(), Some("session closed"));
        // nil errors and unrelated URLs are not errors / tunnels
        let n = parse_ngrok_line(r#"{"lvl":"info","msg":"client session established","err":"<nil>"}"#);
        assert_eq!(n, LineInfo::default());
        let n = parse_ngrok_line(r#"{"lvl":"info","msg":"update available","url":"https://ngrok.com/download"}"#);
        assert_eq!(n.public_url, None);
        // plain text failure
        assert!(parse_ngrok_line("ERROR:  authentication failed: The authtoken is invalid.").error.is_some());
        assert_eq!(parse_ngrok_line("garbage"), LineInfo::default());
    }

    #[test]
    fn ngrok_local_api() {
        let body = json!({"tunnels":[
            {"public_url":"http://x.ngrok-free.app","config":{"addr":"http://localhost:8787"}},
            {"public_url":"https://other.ngrok-free.app","config":{"addr":"http://localhost:9999"}},
            {"public_url":"https://x.ngrok-free.app","config":{"addr":"http://127.0.0.1:8787"}}
        ]});
        assert_eq!(parse_ngrok_api(&body, 8787).as_deref(), Some("https://x.ngrok-free.app"));
        assert_eq!(parse_ngrok_api(&json!({"tunnels":[]}), 8787), None);
        assert_eq!(parse_ngrok_api(&json!({}), 8787), None);
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("cloudflared version 2026.9.0 (built 2026-09-30-1200 UTC)\n").as_deref(), Some("2026.9.0"));
        assert_eq!(parse_version("ngrok version 3.18.4\n").as_deref(), Some("3.18.4"));
        assert_eq!(parse_version("\n\nweird output\n").as_deref(), Some("weird output"));
        assert_eq!(parse_version("   \n"), None);
    }

    // ── command lines ──

    fn cfg_with(provider: Provider) -> TunnelConfig {
        TunnelConfig {
            provider,
            cloudflare_token: s("eyJhIjoiMTIzNDU2Nzg5MCIsInQiOiJhYmNkZWYifQ"),
            ngrok_authtoken: s("2abcdefghijklmnopqrstuvwxyz_7ABCDEFGHIJKLMNOPQRSTUV"),
            ..Default::default()
        }
    }

    #[test]
    fn commands_keep_secrets_off_argv() {
        let q = build_command(&cfg_with(Provider::CloudflareQuick), 8787);
        assert_eq!(q.args, ["tunnel", "--no-autoupdate", "--url", "http://127.0.0.1:8787"]);
        assert!(q.env.is_empty());

        let t = build_command(&cfg_with(Provider::CloudflareToken), 8787);
        assert_eq!(t.args, ["tunnel", "--no-autoupdate", "run"]);
        assert_eq!(t.env, vec![("TUNNEL_TOKEN", cfg_with(Provider::CloudflareToken).cloudflare_token)]);

        let mut c = cfg_with(Provider::Ngrok);
        let n = build_command(&c, 9000);
        assert_eq!(n.args, ["http", "http://127.0.0.1:9000", "--log", "stdout", "--log-format", "json"]);
        assert_eq!(n.env[0].0, "NGROK_AUTHTOKEN");
        c.ngrok_domain = s("https://my-app.ngrok.app/");
        let n = build_command(&c, 9000);
        assert_eq!(&n.args[6..], ["--url", "https://my-app.ngrok.app"]);

        for p in [Provider::CloudflareQuick, Provider::CloudflareToken, Provider::Ngrok] {
            let cfg = cfg_with(p);
            let spec = build_command(&cfg, 8787);
            for secret in cfg.secrets() {
                assert!(spec.args.iter().all(|a| !a.contains(&secret)), "secret leaked into argv for {p:?}");
            }
        }
    }

    #[test]
    fn download_urls() {
        assert_eq!(
            download_url(BinKind::Cloudflared, "x86_64").unwrap(),
            "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64"
        );
        assert_eq!(
            download_url(BinKind::Cloudflared, "aarch64").unwrap(),
            "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-arm64"
        );
        assert_eq!(
            download_url(BinKind::Ngrok, "x86_64").unwrap(),
            "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-linux-amd64.tgz"
        );
        assert!(download_url(BinKind::Ngrok, "riscv64").is_err());
    }

    // ── backoff / health / masking ──

    #[test]
    fn backoff_schedule() {
        let (b, c) = (Duration::from_secs(2), Duration::from_secs(60));
        let got: Vec<u64> = (0..9).map(|a| backoff_delay(a, b, c).as_secs()).collect();
        assert_eq!(got, [2, 4, 8, 16, 32, 60, 60, 60, 60]);
        assert_eq!(backoff_delay(u32::MAX, b, c), c);
    }

    #[test]
    fn health_tracking() {
        let mut t = HealthTracker::new(3);
        assert_eq!(t.record(false), HealthVerdict::Failing(1));
        assert_eq!(t.record(false), HealthVerdict::Failing(2));
        assert_eq!(t.record(true), HealthVerdict::Healthy);
        assert_eq!(t.record(false), HealthVerdict::Failing(1));
        assert_eq!(t.record(false), HealthVerdict::Failing(2));
        assert_eq!(t.record(false), HealthVerdict::Restart);
        assert!(health_ok(200, r#"{"ok":true,"version":"0.2.0"}"#));
        assert!(!health_ok(200, "<html>Cloudflare error 1033</html>"));
        assert!(!health_ok(530, r#"{"ok":true}"#));
        assert!(!health_ok(200, r#"{"ok":false}"#));
    }

    #[test]
    fn masking() {
        let secrets = vec![cfg_with(Provider::Ngrok).cloudflare_token, s("abc")];
        let tok = &secrets[0];
        let line = format!("INF Starting tunnel token={tok} ok");
        let m = mask_secrets(&line, &secrets);
        assert!(!m.contains(tok.as_str()));
        assert!(m.contains(MASK));
        // short "secrets" must not shred the log
        assert_eq!(mask_secrets("abc def", &secrets), "abc def");
        // unknown tokens are caught heuristically
        let m = mask_secrets("connecting with eyJhIjoiMTIzNDU2Nzg5MDEyMzQ1Njc4OTAifQ now", &[]);
        assert_eq!(m, format!("connecting with {MASK} now"));
        let m = mask_secrets(r#"{"authtoken":"2abcdefghijklmnopqrstuvwxyz_7ABCDEFGHIJKLMNOPQRSTUV"}"#, &[]);
        assert!(!m.contains("2abcdefghij"), "{m}");
        let m = mask_secrets("using NGROK token 2abcdefghijklmnopqrstuvwxyz_7ABCDEFGHIJKLMNOPQRSTUV!", &[]);
        assert!(!m.contains("2abcdefghij"), "{m}");
        assert_eq!(mask_secrets("\x1b[32mINF\x1b[0m hello", &[]), "INF hello");
        // ordinary log lines are untouched
        let l = "2026-10-11T03:00:04Z INF Registered tunnel connection connIndex=0 location=nrt01";
        assert_eq!(mask_secrets(l, &[]), l);
    }

    // ── config ──

    #[test]
    fn config_merge_ignores_mask() {
        let mut c = TunnelConfig { cloudflare_token: s("secret-token-123"), ngrok_authtoken: s("ngrok-secret-456"), ..Default::default() };
        c.apply_patch(&json!({"cloudflare_token": MASK, "ngrok_authtoken": MASK, "autostart": true, "provider": "ngrok", "unknown": 1})).unwrap();
        assert_eq!(c.cloudflare_token, "secret-token-123");
        assert_eq!(c.ngrok_authtoken, "ngrok-secret-456");
        assert!(c.autostart);
        assert_eq!(c.provider, Provider::Ngrok);
        c.apply_patch(&json!({"cloudflare_token": "  new-token-xyz \n", "ngrok_domain": "me.ngrok.app"})).unwrap();
        assert_eq!(c.cloudflare_token, "new-token-xyz");
        assert_eq!(c.ngrok_domain, "me.ngrok.app");
        c.apply_patch(&json!({"cloudflare_token": ""})).unwrap();
        assert_eq!(c.cloudflare_token, "");
    }

    #[test]
    fn config_patch_is_all_or_nothing() {
        let mut c = TunnelConfig::default();
        let before = c.clone();
        assert!(c.apply_patch(&json!({"autostart": true, "provider": "carrier-pigeon"})).is_err());
        assert_eq!(c, before);
        assert!(c.apply_patch(&json!({"autostart": "yes"})).is_err());
        assert!(c.apply_patch(&json!([1])).is_err());
        assert_eq!(c, before);
    }

    #[test]
    fn config_masking_and_validation() {
        let c = TunnelConfig { cloudflare_token: s("abc123456"), ..Default::default() };
        let m = c.masked();
        assert_eq!(m.cloudflare_token, MASK);
        assert_eq!(m.ngrok_authtoken, "");
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("abc123456"));

        let mut c = TunnelConfig::default();
        assert!(c.validate().is_ok());
        c.provider = Provider::CloudflareToken;
        assert!(c.validate().unwrap_err().contains("トークン"));
        c.cloudflare_token = s("tok");
        assert!(c.validate().is_ok());
        c.provider = Provider::Ngrok;
        assert!(c.validate().unwrap_err().contains("Authtoken"));
        c.ngrok_authtoken = s("x");
        c.ngrok_domain = s("bad domain!");
        assert!(c.validate().is_err());
        c.ngrok_domain = s("https://ok-domain.ngrok.app/");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn restart_only_when_it_matters() {
        let a = TunnelConfig::default();
        let mut b = a.clone();
        b.autostart = true;
        assert!(!needs_restart(&a, &b));
        b.ngrok_domain = s("x.ngrok.app");
        assert!(needs_restart(&a, &b));
    }

    #[test]
    fn url_normalisation() {
        assert_eq!(normalize_public_url("llm.example.com").as_deref(), Some("https://llm.example.com"));
        assert_eq!(normalize_public_url(" https://llm.example.com/ ").as_deref(), Some("https://llm.example.com"));
        assert_eq!(normalize_public_url("  "), None);
        assert_eq!(normalize_domain("https://a.ngrok.app/x"), "a.ngrok.app");
        assert_eq!(normalize_domain(" a.ngrok.app "), "a.ngrok.app");
    }

    #[test]
    fn config_file_is_private_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/tunnel.json");
        let c = TunnelConfig { provider: Provider::CloudflareToken, cloudflare_token: s("tok-123456"), autostart: true, ..Default::default() };
        write_config_file(&path, &c).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(read_config_file(&path), c);
        // corrupt file -> defaults, no panic
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(read_config_file(&path), TunnelConfig::default());
        assert_eq!(read_config_file(&dir.path().join("missing.json")), TunnelConfig::default());
    }

    // ── logs ──

    #[test]
    fn log_ring_buffer() {
        let mut b = LogBuf::new(3);
        assert_eq!(b.since(0), (vec![], 0));
        for i in 1..=5 {
            b.push(format!("l{i}"));
        }
        let (lines, last) = b.since(0);
        assert_eq!(last, 5);
        assert_eq!(lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [3, 4, 5]);
        let (lines, last) = b.since(4);
        assert_eq!((lines.len(), last), (1, 5));
        assert_eq!(lines[0].text, "l5");
        assert_eq!(b.since(5), (vec![], 5));
    }

    // ── binaries ──

    fn write_exec(path: &Path, body: &str) {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o755).open(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn binary_resolution_prefers_path() {
        let home = tempfile::tempdir().unwrap();
        let pathdir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("bin")).unwrap();
        let empty = OsString::from(pathdir.path());
        assert_eq!(resolve_binary(BinKind::Cloudflared, home.path(), &empty), None);

        write_exec(&home.path().join("bin/cloudflared"), "#!/bin/sh\n");
        assert_eq!(resolve_binary(BinKind::Cloudflared, home.path(), &empty), Some(home.path().join("bin/cloudflared")));

        write_exec(&pathdir.path().join("cloudflared"), "#!/bin/sh\n");
        assert_eq!(resolve_binary(BinKind::Cloudflared, home.path(), &empty), Some(pathdir.path().join("cloudflared")));

        // non-executable files don't count
        std::fs::write(pathdir.path().join("ngrok"), b"x").unwrap();
        std::fs::set_permissions(pathdir.path().join("ngrok"), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(resolve_binary(BinKind::Ngrok, home.path(), &empty), None);
    }

    // ── supervisor integration ──

    fn fast_timings() -> Timings {
        Timings {
            backoff_base: Duration::from_millis(100),
            backoff_cap: Duration::from_millis(400),
            kill_grace: Duration::from_secs(2),
            ..Default::default()
        }
    }

    fn manager(dir: &Path, bin: Option<PathBuf>, auth: bool) -> Arc<TunnelManager> {
        let m = TunnelManager::with_options(
            dir.join("home"),
            8787,
            Options { timings: fast_timings(), cloudflared_bin: bin, ngrok_bin: None, path_var: Some(OsString::new()) },
        );
        m.set_auth_check(Arc::new(move || auth));
        m
    }

    async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    #[tokio::test]
    async fn start_refused_without_auth() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), Some(PathBuf::from("/bin/true")), false);
        let err = m.start().unwrap_err();
        assert_eq!(err.0, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.1.contains("先に認証を設定してください"), "{}", err.1);
        assert_eq!(m.rt.lock().state, TunnelState::Stopped);
        assert!(m.sup.lock().is_none());
        let st = m.status_json().await;
        assert_eq!(st["auth_configured"], json!(false));
        assert_eq!(st["state"], json!("stopped"));
    }

    #[tokio::test]
    async fn start_refused_on_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), Some(PathBuf::from("/bin/true")), true);
        m.update_config(&json!({"provider": "cloudflare_token"})).unwrap();
        let err = m.start().unwrap_err();
        assert_eq!(err.0, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.1.contains("トークン"));
        assert!(m.sup.lock().is_none());
    }

    #[tokio::test]
    async fn status_masks_secrets_and_persists_config() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), None, true);
        m.update_config(&json!({"provider": "ngrok", "ngrok_authtoken": "super-secret-token-1", "autostart": true})).unwrap();
        m.update_config(&json!({"ngrok_authtoken": MASK})).unwrap();
        let st = m.status_json().await;
        assert_eq!(st["config"]["ngrok_authtoken"], json!(MASK));
        assert_eq!(st["config"]["cloudflare_token"], json!(""));
        assert_eq!(st["provider"], json!("ngrok"));
        assert!(!st.to_string().contains("super-secret-token-1"));
        // reload from disk
        let again = TunnelManager::with_options(dir.path().join("home"), 8787, Options::default());
        assert_eq!(again.config().ngrok_authtoken, "super-secret-token-1");
        assert!(again.config().autostart);
    }

    #[tokio::test]
    async fn supervisor_restarts_exited_process() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-cloudflared");
        write_exec(
            &script,
            "#!/bin/sh\n\
             echo '2026-10-11T03:00:00Z INF |  https://quiet-river-1234.trycloudflare.com                                  |'\n\
             echo '2026-10-11T03:00:01Z INF Registered tunnel connection connIndex=0 location=nrt01' >&2\n\
             sleep 1\n\
             exit 1\n",
        );
        let m = manager(dir.path(), Some(script), true);
        m.start().unwrap();
        assert_eq!(m.rt.lock().state, TunnelState::Starting);

        wait_for("connected", || m.rt.lock().state == TunnelState::Connected).await;
        assert_eq!(m.rt.lock().public_url.as_deref(), Some("https://quiet-river-1234.trycloudflare.com"));
        assert!(m.rt.lock().connected_since.is_some());

        wait_for("first restart", || m.rt.lock().restarts >= 1).await;
        let last_error = m.rt.lock().last_error.clone().unwrap_or_default();
        assert!(last_error.contains("プロセスが終了しました"), "{last_error}");
        wait_for("second restart", || m.rt.lock().restarts >= 2).await;
        // and it came back up in between
        wait_for("reconnected", || m.rt.lock().state == TunnelState::Connected).await;

        let logs = m.logs_since(0);
        let texts: Vec<&str> = logs["lines"].as_array().unwrap().iter().map(|l| l["text"].as_str().unwrap()).collect();
        assert!(texts.iter().any(|t| t.contains("quiet-river-1234.trycloudflare.com")));
        assert!(texts.iter().any(|t| t.contains("Registered tunnel connection")));
        assert!(logs["last"].as_u64().unwrap() >= texts.len() as u64);

        m.stop().await;
        assert_eq!(m.rt.lock().state, TunnelState::Stopped);
        assert!(m.sup.lock().is_none());
        let r = m.rt.lock().restarts;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(m.rt.lock().restarts, r, "no restarts after a manual stop");
        assert_eq!(m.rt.lock().state, TunnelState::Stopped);
    }

    #[tokio::test]
    async fn start_timeout_restarts_hung_process() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hang");
        write_exec(&script, "#!/bin/sh\necho 'INF starting'\nsleep 30\n");
        let m = TunnelManager::with_options(
            dir.path().join("home"),
            8787,
            Options {
                timings: Timings { start_timeout: Duration::from_millis(400), ..fast_timings() },
                cloudflared_bin: Some(script),
                ngrok_bin: None,
                path_var: Some(OsString::new()),
            },
        );
        m.set_auth_check(Arc::new(|| true));
        m.start().unwrap();
        wait_for("start-timeout restart", || m.rt.lock().restarts >= 1).await;
        assert!(m.rt.lock().last_error.clone().unwrap().contains("接続できなかった"));
        m.stop().await;
    }

    #[tokio::test]
    async fn spawn_failure_is_retried_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), Some(dir.path().join("does-not-exist")), true);
        m.start().unwrap();
        wait_for("retries", || m.rt.lock().restarts >= 2).await;
        assert!(m.rt.lock().last_error.clone().unwrap().contains("起動できません"));
        m.stop().await;
    }

    #[tokio::test]
    async fn config_change_reloads_running_tunnel_without_counting_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("ok");
        write_exec(
            &script,
            "#!/bin/sh\necho 'INF https://calm-fox-9.trycloudflare.com'\necho 'INF Registered tunnel connection' >&2\nsleep 30\n",
        );
        let m = manager(dir.path(), Some(script), true);
        m.start().unwrap();
        wait_for("connected", || m.rt.lock().state == TunnelState::Connected).await;
        let seq = m.logs_since(0)["last"].as_u64().unwrap();
        // token mode without token -> terminal error, not a crash loop
        m.update_config(&json!({"provider": "cloudflare_token"})).unwrap();
        wait_for("error state", || m.rt.lock().state == TunnelState::Error).await;
        assert!(m.rt.lock().last_error.clone().unwrap().contains("トークン"));
        assert_eq!(m.rt.lock().restarts, 0);
        assert!(m.logs_since(seq)["lines"].as_array().unwrap().iter().any(|l| l["text"].as_str().unwrap().contains("再起動")));
        m.stop().await;
    }

    #[tokio::test]
    async fn secrets_travel_via_env_and_are_masked_in_logs() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("env-echo");
        // prints the token it received, like a verbose tool might
        write_exec(&script, "#!/bin/sh\necho \"INF token is $TUNNEL_TOKEN args=$*\"\necho 'INF Registered tunnel connection' >&2\nsleep 30\n");
        let m = manager(dir.path(), Some(script), true);
        m.update_config(&json!({"provider": "cloudflare_token", "cloudflare_token": "eyJsecretsecretsecretsecret99", "cloudflare_hostname": "llm.example.com"})).unwrap();
        m.start().unwrap();
        wait_for("connected", || m.rt.lock().state == TunnelState::Connected).await;
        assert_eq!(m.rt.lock().public_url.as_deref(), Some("https://llm.example.com"));
        let all = m.logs_since(0).to_string();
        assert!(all.contains("token is"), "{all}");
        assert!(!all.contains("eyJsecretsecretsecretsecret99"), "{all}");
        assert!(all.contains(MASK));
        m.stop().await;
    }
}
