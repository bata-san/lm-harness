//! Authentication for remote access (Cloudflare Tunnel / ngrok).
//!
//! Design goals: never fail open. Every uncertain situation (corrupt state file, missing
//! connection info, unparsable data) resolves to "deny".
//!
//! * Storage: `HARNESS_HOME/auth.json` (0600, atomic writes). Tokens and API keys are only
//!   stored as SHA-256 hashes; the password as an argon2id PHC string.
//! * Sessions: opaque random cookie `harness_session` (HttpOnly, SameSite=Strict), 7 day sliding
//!   idle expiry, 30 day absolute expiry.
//! * API keys: `sk-lh-...` (OpenAI style), `sk-ant-lh-...` (Anthropic style) or legacy `lh_...`,
//!   accepted as `Authorization: Bearer <key>` or `x-api-key: <key>`. A key may ONLY call `/v1/*`
//!   (every other route is session-only; `/api/health` is public anyway). Failed key attempts are
//!   rate limited per IP like failed logins.
//! * CSRF: cookie-authenticated unsafe requests must carry `X-Harness-CSRF: 1`.
//! * Local setup: first-run account creation is only possible from the workstation itself
//!   (loopback peer, no proxy headers, loopback Host).
//! * The file is re-read when it changes on disk, so `llm-harness auth ...` CLI recovery takes
//!   effect immediately in a running server (and revokes sessions).

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRef, FromRequest, FromRequestParts, Path as UrlPath, Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE, USER_AGENT};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use parking_lot::{Mutex, RwLock};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::HARNESS_HOME;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// constants

pub const SESSION_COOKIE: &str = "harness_session";
pub const CSRF_HEADER: &str = "x-harness-csrf";
const COOKIE_MAX_AGE: i64 = 30 * 24 * 3600;
const SESSION_IDLE_SECS: i64 = 7 * 24 * 3600;
const SESSION_ABSOLUTE_SECS: i64 = 30 * 24 * 3600;
/// Minimum interval between persisted `last_seen_at` / `last_used_at` updates.
const TOUCH_INTERVAL_SECS: i64 = 60;
const MAX_SESSIONS: usize = 50;
const MAX_API_KEYS: usize = 50;
const MIN_PASSWORD_CHARS: usize = 12;
const MAX_PASSWORD_BYTES: usize = 1024;
const RATE_WINDOW: Duration = Duration::from_secs(15 * 60);
const RATE_MAX_PER_IP: usize = 5;
const RATE_MAX_GLOBAL: usize = 30;
const RELOAD_INTERVAL: Duration = if cfg!(test) { Duration::ZERO } else { Duration::from_secs(1) };
const TOKEN_LEN: usize = 43; // base64url(32 bytes)
const API_KEY_PREFIX: &str = "lh_";
const OPENAI_KEY_PREFIX: &str = "sk-lh-";
const ANTHROPIC_KEY_PREFIX: &str = "sk-ant-lh-";

/// Key prefix of a kind ("openai" | "anthropic" | "generic").
fn key_prefix_of(kind: &str) -> Option<&'static str> {
    match kind {
        "openai" => Some(OPENAI_KEY_PREFIX),
        "anthropic" => Some(ANTHROPIC_KEY_PREFIX),
        "generic" => Some(API_KEY_PREFIX),
        _ => None,
    }
}

/// Generate a fresh key of `kind` (full secret; show it once).
fn generate_key(kind: &str) -> Option<String> {
    Some(format!("{}{}", key_prefix_of(kind)?, random_token()))
}

/// Shape check: right prefix and length for one of the three kinds.
fn key_has_valid_shape(key: &str) -> bool {
    [OPENAI_KEY_PREFIX, ANTHROPIC_KEY_PREFIX, API_KEY_PREFIX].iter().any(|p| key.strip_prefix(p).is_some_and(|rest| rest.len() == TOKEN_LEN))
}

fn default_key_kind() -> String {
    "generic".into()
}

const MSG_UNCONFIGURED: &str = "認証が未設定です。ワークステーション上で初期設定してください";
const MSG_LOGIN_REQUIRED: &str = "ログインが必要です";
const MSG_CORRUPT: &str = "認証ファイルを読み込めません。ワークステーション上で `llm-harness auth set-password` を実行して復旧してください";
const MSG_BAD_LOGIN: &str = "ユーザー名またはパスワードが違います";
const MSG_TOTP_REQUIRED: &str = "二段階認証コードが必要です";
const MSG_TOTP_WRONG: &str = "二段階認証コードが違います";

// ---------------------------------------------------------------------------------------------
// persisted model

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AuthFile {
    username: String,
    password_hash: String,
    #[serde(default)]
    totp: TotpState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_totp_secret: Option<String>,
    #[serde(default)]
    api_keys: Vec<ApiKeyRec>,
    #[serde(default)]
    sessions: Vec<SessionRec>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct TotpState {
    #[serde(default)]
    secret_b32: String,
    #[serde(default)]
    enabled: bool,
    /// Last accepted time step (replay protection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_step: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApiKeyRec {
    id: String,
    name: String,
    prefix: String,
    /// "openai" | "anthropic" | "generic" (keys written before kinds existed are generic).
    #[serde(default = "default_key_kind")]
    kind: String,
    /// sha256 hex of the full key.
    hash: String,
    created_at: i64,
    #[serde(default)]
    last_used_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionRec {
    id: String,
    /// sha256 hex of the cookie token.
    token_hash: String,
    created_at: i64,
    last_seen_at: i64,
    #[serde(default)]
    ip: String,
    #[serde(default)]
    user_agent: String,
}

impl SessionRec {
    fn is_live(&self, now: i64) -> bool {
        now - self.created_at < SESSION_ABSOLUTE_SECS && now - self.last_seen_at < SESSION_IDLE_SECS
    }
}

// ---------------------------------------------------------------------------------------------
// manager

/// File identity used to notice external modifications (CLI recovery).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileSig {
    ino: u64,
    mtime_ns: i128,
    len: u64,
}

enum Store {
    /// No auth file exists: first-run setup is possible (locally).
    Unconfigured,
    /// File exists but cannot be trusted. Everything is locked except local CLI recovery.
    Corrupt,
    Ready(Box<AuthFile>),
}

struct Inner {
    store: Store,
    sig: Option<FileSig>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Unconfigured,
    Corrupt,
    Configured,
}

pub struct AuthManager {
    path: PathBuf,
    inner: RwLock<Inner>,
    limiter: Mutex<Limiter>,
    /// Failed API-key attempts (separate from logins so key spam cannot lock out the admin).
    key_limiter: Mutex<Limiter>,
    last_reload_check: Mutex<Instant>,
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

fn ts(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn file_sig(path: &Path) -> Option<FileSig> {
    let md = std::fs::metadata(path).ok()?;
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&md);
    #[cfg(not(unix))]
    let ino = 0;
    Some(FileSig { ino, mtime_ns, len: md.len() })
}

/// `Ok(None)` = file does not exist; `Err` = exists but unusable.
fn read_auth_file(path: &Path) -> Result<Option<AuthFile>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read failed: {e}")),
    };
    let file: AuthFile = serde_json::from_slice(&bytes).map_err(|e| format!("parse failed: {e}"))?;
    if file.username.is_empty() {
        return Err("empty username".into());
    }
    PasswordHash::new(&file.password_hash).map_err(|e| format!("invalid password hash: {e}"))?;
    if file.totp.enabled && base32_decode(&file.totp.secret_b32).is_none_or(|s| s.is_empty()) {
        return Err("invalid totp secret".into());
    }
    Ok(Some(file))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_auth_file(path: &Path, file: &AuthFile) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(file)?;
    write_atomic(path, &bytes)?;
    Ok(())
}

impl AuthManager {
    /// Load `HARNESS_HOME/auth.json`. Never panics; a corrupt file locks the panel down.
    pub fn load() -> Arc<Self> {
        Self::load_from(HARNESS_HOME.join("auth.json"))
    }

    /// Load from an explicit path (tests, tooling).
    pub fn load_from(path: PathBuf) -> Arc<Self> {
        let inner = Self::read_inner(&path);
        Arc::new(Self {
            path,
            inner: RwLock::new(inner),
            limiter: Mutex::new(Limiter::default()),
            key_limiter: Mutex::new(Limiter::default()),
            last_reload_check: Mutex::new(Instant::now()),
        })
    }

    fn read_inner(path: &Path) -> Inner {
        let sig = file_sig(path);
        let store = match read_auth_file(path) {
            Ok(None) => Store::Unconfigured,
            Ok(Some(mut f)) => {
                let now = now_unix();
                f.sessions.retain(|s| s.is_live(now));
                Store::Ready(Box::new(f))
            }
            Err(e) => {
                tracing::error!(
                    "認証ファイル {} を読み込めません ({e})。安全のため全てのリクエストを拒否します。\
                     `llm-harness auth set-password` で復旧してください。",
                    path.display()
                );
                Store::Corrupt
            }
        };
        Inner { store, sig }
    }

    /// True once an admin user/password exists (a corrupt file counts as configured).
    pub fn is_configured(&self) -> bool {
        !matches!(self.inner.read().store, Store::Unconfigured)
    }

    fn mode(&self) -> Mode {
        match self.inner.read().store {
            Store::Unconfigured => Mode::Unconfigured,
            Store::Corrupt => Mode::Corrupt,
            Store::Ready(_) => Mode::Configured,
        }
    }

    /// Re-read the file if it changed on disk (CLI recovery in another process). A vanished file
    /// is ignored: deleting auth.json must never turn the panel back into "unconfigured".
    pub fn reload_if_changed(&self) {
        {
            let mut last = self.last_reload_check.lock();
            if last.elapsed() < RELOAD_INTERVAL {
                return;
            }
            *last = Instant::now();
        }
        let now_sig = file_sig(&self.path);
        if now_sig.is_none() || now_sig == self.inner.read().sig {
            return;
        }
        let mut inner = self.inner.write();
        if now_sig == inner.sig {
            return;
        }
        *inner = Self::read_inner(&self.path);
        tracing::info!("auth.json changed on disk: reloaded");
    }

    /// Mutate the configured file and persist; rolls back the in-memory change if the write fails.
    fn mutate<R>(&self, f: impl FnOnce(&mut AuthFile) -> R) -> Result<R, ApiError> {
        let mut inner = self.inner.write();
        let Inner { store, sig } = &mut *inner;
        let Store::Ready(file) = store else {
            return Err(ApiError(StatusCode::UNAUTHORIZED, MSG_LOGIN_REQUIRED.into()));
        };
        let backup = (**file).clone();
        let r = f(file);
        if let Err(e) = write_auth_file(&self.path, file) {
            tracing::error!("auth.json write failed: {e:#}");
            **file = backup;
            return Err(ApiError::internal("認証情報を保存できませんでした"));
        }
        *sig = file_sig(&self.path);
        Ok(r)
    }

    /// Best-effort persist of a debounced timestamp update.
    fn persist_lazy(&self, inner: &mut Inner) {
        if let Store::Ready(file) = &inner.store {
            match write_auth_file(&self.path, file) {
                Ok(()) => inner.sig = file_sig(&self.path),
                Err(e) => tracing::warn!("auth.json write failed: {e:#}"),
            }
        }
    }

    fn read_file<R>(&self, f: impl FnOnce(&AuthFile) -> R) -> Option<R> {
        match &self.inner.read().store {
            Store::Ready(file) => Some(f(file)),
            _ => None,
        }
    }

    // --- sessions ---------------------------------------------------------------------------

    fn session_principal(&self, token: &str, now: i64) -> Option<Principal> {
        if token.len() != TOKEN_LEN {
            return None;
        }
        let hash = sha256_hex(token.as_bytes());
        let found = self.read_file(|f| {
            let mut found: Option<(String, i64, i64)> = None;
            for s in &f.sessions {
                // No early exit: compare against every stored hash.
                if bool::from(s.token_hash.as_bytes().ct_eq(hash.as_bytes())) && s.is_live(now) {
                    found = Some((s.id.clone(), s.last_seen_at, s.created_at));
                }
            }
            (found, f.username.clone())
        })?;
        let ((id, last_seen, _), user) = (found.0?, found.1);
        if now - last_seen >= TOUCH_INTERVAL_SECS {
            let mut inner = self.inner.write();
            let mut changed = false;
            if let Store::Ready(file) = &mut inner.store {
                if let Some(s) = file.sessions.iter_mut().find(|s| s.id == id) {
                    if now - s.last_seen_at >= TOUCH_INTERVAL_SECS {
                        s.last_seen_at = now;
                        changed = true;
                    }
                }
            }
            if changed {
                self.persist_lazy(&mut inner);
            }
        }
        Some(Principal { kind: "session", name: user, session_id: Some(id) })
    }

    fn create_session(&self, ip: &str, user_agent: &str) -> Result<(String, String), ApiError> {
        let token = random_token();
        let id = random_hex(16);
        let now = now_unix();
        let rec = SessionRec {
            id: id.clone(),
            token_hash: sha256_hex(token.as_bytes()),
            created_at: now,
            last_seen_at: now,
            ip: ip.chars().take(64).collect(),
            user_agent: user_agent.chars().take(200).collect(),
        };
        self.mutate(|f| {
            f.sessions.retain(|s| s.is_live(now));
            f.sessions.push(rec);
            while f.sessions.len() > MAX_SESSIONS {
                if let Some(idx) = f.sessions.iter().enumerate().min_by_key(|(_, s)| s.created_at).map(|(i, _)| i) {
                    f.sessions.remove(idx);
                }
            }
        })?;
        Ok((token, id))
    }

    // --- API keys ---------------------------------------------------------------------------

    fn api_key_principal(&self, key: &str, now: i64) -> Option<Principal> {
        if !key_has_valid_shape(key) {
            return None;
        }
        let hash = sha256_hex(key.as_bytes());
        let (found, _) = self.read_file(|f| {
            let mut found: Option<(String, String, Option<i64>)> = None;
            for k in &f.api_keys {
                if bool::from(k.hash.as_bytes().ct_eq(hash.as_bytes())) {
                    found = Some((k.id.clone(), k.name.clone(), k.last_used_at));
                }
            }
            (found, ())
        })?;
        let (id, name, last_used) = found?;
        if last_used.is_none_or(|t| now - t >= TOUCH_INTERVAL_SECS) {
            let mut inner = self.inner.write();
            let mut changed = false;
            if let Store::Ready(file) = &mut inner.store {
                if let Some(k) = file.api_keys.iter_mut().find(|k| k.id == id) {
                    if k.last_used_at.is_none_or(|t| now - t >= TOUCH_INTERVAL_SECS) {
                        k.last_used_at = Some(now);
                        changed = true;
                    }
                }
            }
            if changed {
                self.persist_lazy(&mut inner);
            }
        }
        Some(Principal { kind: "api_key", name, session_id: None })
    }

    fn username(&self) -> Option<String> {
        self.read_file(|f| f.username.clone())
    }

    // --- TOTP -------------------------------------------------------------------------------

    /// Verify a login code against the active secret and consume its time step (replay protection).
    fn consume_totp(&self, code: &str, now: i64) -> Result<bool, ApiError> {
        let mut inner = self.inner.write();
        let Inner { store, sig } = &mut *inner;
        let Store::Ready(file) = store else { return Ok(false) };
        if !file.totp.enabled {
            return Ok(false);
        }
        let Some(secret) = base32_decode(&file.totp.secret_b32) else { return Ok(false) };
        let Some(step) = verify_totp(&secret, code, now, file.totp.last_step) else { return Ok(false) };
        let prev = file.totp.last_step;
        file.totp.last_step = Some(step);
        if let Err(e) = write_auth_file(&self.path, file) {
            tracing::error!("auth.json write failed: {e:#}");
            file.totp.last_step = prev;
            return Err(ApiError::internal("認証情報を保存できませんでした"));
        }
        *sig = file_sig(&self.path);
        Ok(true)
    }

    // --- rate limiting ----------------------------------------------------------------------

    fn rate_check(&self, ip: IpAddr) -> Result<(), ApiError> {
        match self.limiter.lock().check(ip_key(ip), Instant::now()) {
            None => Ok(()),
            Some(mins) => Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                format!("ログイン試行が多すぎます。{mins} 分後に再試行してください"),
            )),
        }
    }
    fn rate_fail(&self, ip: IpAddr) {
        self.limiter.lock().fail(ip_key(ip), Instant::now());
    }
    /// Record a failed key attempt; `Err(429)` once the IP (or everyone) is over the limit.
    fn key_fail(&self, ip: IpAddr) -> Result<(), ApiError> {
        let mut l = self.key_limiter.lock();
        let now = Instant::now();
        if let Some(mins) = l.check(ip_key(ip), now) {
            return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, format!("API キーの認証失敗が多すぎます。{mins} 分後に再試行してください")));
        }
        l.fail(ip_key(ip), now);
        Ok(())
    }
    fn rate_reset(&self, ip: IpAddr) {
        self.limiter.lock().reset(ip_key(ip));
    }
}

// ---------------------------------------------------------------------------------------------
// rate limiter

#[derive(Default)]
struct Limiter {
    per_ip: HashMap<IpAddr, VecDeque<Instant>>,
    global: VecDeque<Instant>,
}

fn ip_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            // One bucket per /64 so rotating addresses inside a prefix does not help.
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
        v4 => v4,
    }
}

fn prune(q: &mut VecDeque<Instant>, now: Instant) {
    while q.front().is_some_and(|t| now.duration_since(*t) >= RATE_WINDOW) {
        q.pop_front();
    }
}

fn retry_minutes(q: &VecDeque<Instant>, now: Instant) -> u64 {
    let oldest = q.front().copied().unwrap_or(now);
    let left = (oldest + RATE_WINDOW).saturating_duration_since(now).as_secs();
    left.div_ceil(60).max(1)
}

impl Limiter {
    /// `Some(minutes)` when the attempt must be rejected.
    fn check(&mut self, ip: IpAddr, now: Instant) -> Option<u64> {
        prune(&mut self.global, now);
        let mut wait: Option<u64> = None;
        if self.global.len() >= RATE_MAX_GLOBAL {
            wait = Some(retry_minutes(&self.global, now));
        }
        if let Some(q) = self.per_ip.get_mut(&ip) {
            prune(q, now);
            if q.len() >= RATE_MAX_PER_IP {
                wait = Some(wait.unwrap_or(0).max(retry_minutes(q, now)));
            }
            if q.is_empty() {
                self.per_ip.remove(&ip);
            }
        }
        wait
    }

    fn fail(&mut self, ip: IpAddr, now: Instant) {
        prune(&mut self.global, now);
        self.global.push_back(now);
        if self.per_ip.len() > 4096 {
            self.per_ip.retain(|_, q| {
                prune(q, now);
                !q.is_empty()
            });
        }
        let q = self.per_ip.entry(ip).or_default();
        prune(q, now);
        q.push_back(now);
    }

    fn reset(&mut self, ip: IpAddr) {
        self.per_ip.remove(&ip);
    }
}

// ---------------------------------------------------------------------------------------------
// crypto helpers

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn argon2_params() -> Params {
    // Tests use cheap parameters; production uses 64 MiB / 3 passes (above OWASP minimums).
    let (m, t) = if cfg!(test) { (8, 1) } else { (64 * 1024, 3) };
    Params::new(m, t, 1, None).expect("valid argon2 params")
}

fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::encode_b64(&random_bytes::<16>()).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon2_params());
    let hash = argon.hash_password(password.as_bytes(), &salt).map_err(|e| anyhow::anyhow!("hash: {e}"))?;
    Ok(hash.to_string())
}

fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// A hash of a random password, used to equalise timing for unknown users.
fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| hash_password(&random_token()).unwrap_or_else(|_| String::new()));
    DUMMY.as_str()
}

fn validate_new_password(pw: &str) -> Result<(), ApiError> {
    if pw.len() > MAX_PASSWORD_BYTES {
        return Err(ApiError::bad_request("パスワードが長すぎます"));
    }
    if pw.chars().count() < MIN_PASSWORD_CHARS {
        return Err(ApiError::bad_request(format!("パスワードは {MIN_PASSWORD_CHARS} 文字以上にしてください")));
    }
    Ok(())
}

fn validate_username(u: &str) -> Result<(), ApiError> {
    let ok = !u.is_empty() && u.len() <= 32 && u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request("ユーザー名は 1〜32 文字の英数字・_ . - のみ使えます"))
    }
}

// --- base32 / TOTP -------------------------------------------------------------------------

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for c in s.chars() {
        if matches!(c, '=' | ' ' | '-') {
            continue;
        }
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u32 - 'A' as u32,
            c @ '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// RFC 4226 HOTP with SHA-1, 6 digits, for an explicit counter (= RFC 6238 time step).
pub fn totp_at(secret: &[u8], step: u64) -> String {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = (h[19] & 0x0f) as usize;
    let bin = ((h[off] as u32 & 0x7f) << 24) | ((h[off + 1] as u32) << 16) | ((h[off + 2] as u32) << 8) | (h[off + 3] as u32);
    format!("{:06}", bin % 1_000_000)
}

/// Check `code` for steps now-1..=now+1; only steps strictly greater than `last_step` are accepted.
/// Returns the matched step.
pub fn verify_totp(secret: &[u8], code: &str, now: i64, last_step: Option<u64>) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) || now < 0 {
        return None;
    }
    let cur = (now as u64) / 30;
    let mut matched: Option<u64> = None;
    for step in cur.saturating_sub(1)..=cur + 1 {
        let expect = totp_at(secret, step);
        // Evaluate all candidates; no early exit.
        if bool::from(expect.as_bytes().ct_eq(code.as_bytes())) && last_step.is_none_or(|l| step > l) {
            matched = Some(step);
        }
    }
    matched
}

// ---------------------------------------------------------------------------------------------
// request classification

#[derive(Clone, Debug)]
pub struct Principal {
    /// "session" | "api_key" | "local"
    pub kind: &'static str,
    pub name: String,
    pub session_id: Option<String>,
}

const FORWARD_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-forwarded-server",
    "x-original-forwarded-for",
    "cf-connecting-ip",
    "cf-connecting-ipv6",
    "cf-ray",
    "cf-visitor",
    "cf-ipcountry",
    "x-real-ip",
    "true-client-ip",
    "via",
];

fn has_forwarding_headers(h: &HeaderMap) -> bool {
    h.keys().any(|k| {
        let n = k.as_str();
        FORWARD_HEADERS.contains(&n) || n.starts_with("ngrok-") || n.starts_with("cf-")
    })
}

fn host_is_loopback(headers: &HeaderMap, uri: &Uri) -> bool {
    let raw = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok().map(str::to_owned))
        .or_else(|| uri.authority().map(|a| a.as_str().to_owned()));
    let Some(raw) = raw else { return true };
    let host = if let Some(rest) = raw.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_owned()
    } else {
        raw.rsplit_once(':').map(|(h, p)| if p.bytes().all(|b| b.is_ascii_digit()) { h } else { raw.as_str() }).unwrap_or(&raw).to_owned()
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// Local = the request really originates on this machine: loopback peer, no proxy/tunnel
/// headers (tunnels connect from loopback but always add them), and a loopback `Host`
/// (defeats DNS rebinding from a browser on the workstation).
pub fn is_local_parts(peer: Option<SocketAddr>, headers: &HeaderMap, uri: &Uri) -> bool {
    peer.is_some_and(|p| p.ip().to_canonical().is_loopback()) && !has_forwarding_headers(headers) && host_is_loopback(headers, uri)
}

#[allow(dead_code)] // public helper for other modules
pub fn is_local<B>(req: &axum::http::Request<B>) -> bool {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    is_local_parts(peer, req.headers(), req.uri())
}

fn parse_ip(s: &str) -> Option<IpAddr> {
    let s = s.trim().trim_matches('"');
    s.parse::<IpAddr>()
        .ok()
        .or_else(|| s.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .or_else(|| s.strip_prefix('[').and_then(|r| r.strip_suffix(']')).and_then(|r| r.parse().ok()))
}

/// Client IP for rate limiting / session records: CF-Connecting-IP, X-Forwarded-For (first
/// entry), X-Real-IP, else the peer address.
pub fn client_ip(peer: Option<SocketAddr>, headers: &HeaderMap) -> IpAddr {
    for name in ["cf-connecting-ip", "x-forwarded-for", "x-real-ip"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            if let Some(ip) = v.split(',').next().and_then(parse_ip) {
                return ip;
            }
        }
    }
    peer.map(|p| p.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]))
}

fn request_is_https(headers: &HeaderMap) -> bool {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').next().is_some_and(|p| p.trim().eq_ignore_ascii_case("https")));
    let cf = headers.get("cf-visitor").and_then(|v| v.to_str().ok()).is_some_and(|v| v.to_ascii_lowercase().contains("https"));
    proto || cf
}

/// Connection facts derived from a request, available as an extractor.
#[derive(Clone, Debug)]
pub struct Client {
    pub ip: IpAddr,
    pub local: bool,
    pub https: bool,
    pub user_agent: String,
}

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
        Ok(Client {
            ip: client_ip(peer, &parts.headers),
            local: is_local_parts(peer, &parts.headers, &parts.uri),
            https: request_is_https(&parts.headers),
            user_agent: parts.headers.get(USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").chars().take(200).collect(),
        })
    }
}

fn cookie_tokens(headers: &HeaderMap) -> Vec<String> {
    let mut out = Vec::new();
    for v in headers.get_all(COOKIE) {
        let Ok(s) = v.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, v)) = part.trim().split_once('=') {
                if k == SESSION_COOKIE {
                    out.push(v.trim().to_owned());
                }
            }
        }
    }
    out
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim().to_owned())
}

/// Every API-key credential presented: `Authorization: Bearer` and `x-api-key`.
fn key_candidates(headers: &HeaderMap) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(bearer_token(headers));
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        out.push(v.trim().to_owned());
    }
    out.retain(|k| !k.is_empty());
    out
}

fn is_v1_path(path: &str) -> bool {
    path == "/v1" || path.starts_with("/v1/")
}

fn is_public(method: &Method, path: &str) -> bool {
    match path {
        "/api/auth/login" | "/api/auth/setup" => method == Method::POST,
        "/api/auth/status" => method == Method::GET || method == Method::HEAD,
        _ => false,
    }
}

fn is_safe_method(m: &Method) -> bool {
    matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Core access decision (independent of AppState so it can be tested in isolation).
/// `Ok(Some(p))` = authenticated, `Ok(None)` = public path, anonymous.
pub fn authorize(
    mgr: &AuthManager,
    peer: Option<SocketAddr>,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<Option<Principal>, ApiError> {
    mgr.reload_if_changed();
    let public = is_public(method, uri.path());
    let deny = |status: StatusCode, msg: &str| Err(ApiError(status, msg.to_owned()));
    match mgr.mode() {
        Mode::Corrupt => {
            if public {
                Ok(None)
            } else {
                deny(StatusCode::UNAUTHORIZED, MSG_CORRUPT)
            }
        }
        Mode::Unconfigured => {
            if public {
                Ok(None)
            } else if is_local_parts(peer, headers, uri) {
                Ok(Some(Principal { kind: "local", name: "local".into(), session_id: None }))
            } else {
                deny(StatusCode::UNAUTHORIZED, MSG_UNCONFIGURED)
            }
        }
        Mode::Configured => {
            let now = now_unix();
            for token in cookie_tokens(headers) {
                if let Some(p) = mgr.session_principal(&token, now) {
                    if !public && !is_safe_method(method) && headers.get(CSRF_HEADER).map(|v| v.as_bytes()) != Some(b"1") {
                        return deny(StatusCode::FORBIDDEN, "CSRF 対策ヘッダー (X-Harness-CSRF) が必要です");
                    }
                    return Ok(Some(p));
                }
            }
            if !public {
                let candidates = key_candidates(headers);
                for key in &candidates {
                    if let Some(p) = mgr.api_key_principal(key, now) {
                        // API keys are for the model API only; all admin routes are session-only.
                        if is_v1_path(uri.path()) {
                            return Ok(Some(p));
                        }
                        return deny(StatusCode::FORBIDDEN, "API キーでは /v1 以外にアクセスできません");
                    }
                }
                if !candidates.is_empty() {
                    mgr.key_fail(client_ip(peer, headers))?;
                }
            }
            if public {
                Ok(None)
            } else {
                deny(StatusCode::UNAUTHORIZED, MSG_LOGIN_REQUIRED)
            }
        }
    }
}

/// Middleware body shared by `require_auth` and the tests.
pub async fn guard(mgr: &AuthManager, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let headers = req.headers().clone();
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    match authorize(mgr, peer, req.method(), req.uri(), req.headers()) {
        Ok(principal) => {
            if let Some(p) = principal {
                req.extensions_mut().insert(p);
            }
            next.run(req).await
        }
        Err(e) => {
            if is_v1_path(&path) {
                v1_error(e, &path, &headers)
            } else {
                e.into_response()
            }
        }
    }
}

/// `/v1/*` clients expect their own error envelope, not the admin `{"detail": ...}` one.
fn v1_error(e: ApiError, path: &str, headers: &HeaderMap) -> Response {
    let anthropic = path.starts_with("/v1/messages") || headers.contains_key("anthropic-version");
    let (atype, otype) = match e.0 {
        StatusCode::UNAUTHORIZED => ("authentication_error", "invalid_request_error"),
        StatusCode::FORBIDDEN => ("permission_error", "invalid_request_error"),
        StatusCode::TOO_MANY_REQUESTS => ("rate_limit_error", "rate_limit_error"),
        _ => ("api_error", "server_error"),
    };
    let body = if anthropic {
        json!({"type": "error", "error": {"type": atype, "message": e.1}})
    } else {
        json!({"error": {"message": e.1, "type": otype, "param": null, "code": atype}})
    };
    (e.0, Json(body)).into_response()
}

/// Axum middleware guarding every `/api/*` and `/v1/*` route (see module docs for rules).
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    guard(&state.auth, req, next).await
}

/// Adds X-Frame-Options / nosniff / Referrer-Policy etc. to every response.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let api = {
        let p = req.uri().path();
        p.starts_with("/api/") || p.starts_with("/v1/") || p == "/api"
    };
    let https = request_is_https(req.headers());
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("permissions-policy", HeaderValue::from_static("camera=(), microphone=(), geolocation=()"));
    if https {
        h.insert("strict-transport-security", HeaderValue::from_static("max-age=31536000"));
    }
    if api {
        h.insert("cache-control", HeaderValue::from_static("no-store"));
    }
    res
}

// ---------------------------------------------------------------------------------------------
// CLI

/// CLI: `llm-harness auth set-password --user <name>` (password from stdin). Works only locally.
pub fn cli_set_password(user: &str, password: &str) -> anyhow::Result<()> {
    cli_set_password_at(&HARNESS_HOME.join("auth.json"), user, password)
}

pub fn cli_set_password_at(path: &Path, user: &str, password: &str) -> anyhow::Result<()> {
    validate_username(user).map_err(|e| anyhow::anyhow!(e.1))?;
    validate_new_password(password).map_err(|e| anyhow::anyhow!(e.1))?;
    let hash = hash_password(password)?;
    let mut file = match read_auth_file(path) {
        Ok(Some(f)) => f,
        // Missing or corrupt: start fresh (this is the recovery path).
        Ok(None) | Err(_) => AuthFile {
            username: user.to_owned(),
            password_hash: String::new(),
            totp: TotpState::default(),
            pending_totp_secret: None,
            api_keys: vec![],
            sessions: vec![],
        },
    };
    file.username = user.to_owned();
    file.password_hash = hash;
    file.sessions.clear();
    write_auth_file(path, &file)
}

/// CLI: `llm-harness auth disable-totp`.
pub fn cli_disable_totp() -> anyhow::Result<()> {
    cli_disable_totp_at(&HARNESS_HOME.join("auth.json"))
}

pub fn cli_disable_totp_at(path: &Path) -> anyhow::Result<()> {
    let mut file = match read_auth_file(path) {
        Ok(Some(f)) => f,
        Ok(None) => anyhow::bail!("認証が未設定です"),
        Err(e) => anyhow::bail!("認証ファイルを読み込めません ({e})。先に `auth set-password` を実行してください"),
    };
    file.totp = TotpState::default();
    file.pending_totp_secret = None;
    write_auth_file(path, &file)
}

// ---------------------------------------------------------------------------------------------
// HTTP layer

impl FromRef<AppState> for Arc<AuthManager> {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

pub fn router() -> Router<AppState> {
    routes::<AppState>()
}

/// All `/api/auth/*` routes, generic over the application state so they can be tested alone.
pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    Arc<AuthManager>: FromRef<S>,
{
    Router::new()
        .route("/api/auth/status", get(status))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/password", post(change_password))
        .route("/api/auth/totp/setup", post(totp_setup))
        .route("/api/auth/totp/enable", post(totp_enable))
        .route("/api/auth/totp/disable", post(totp_disable))
        .route("/api/auth/sessions", get(list_sessions))
        .route("/api/auth/sessions/{id}", delete(delete_session))
        .route("/api/auth/keys", post(create_key).get(list_keys))
        .route("/api/auth/keys/{id}", delete(delete_key))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

/// JSON body extractor that reports errors in the `{"detail": ...}` shape.
struct ApiJson<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(rej) => Err(ApiError(rej.status(), "リクエストの形式が不正です".into())),
        }
    }
}

/// Authenticated *browser session* (API keys and the local principal may not manage credentials).
struct SessionUser {
    session_id: String,
    name: String,
}

impl<S: Send + Sync> FromRequestParts<S> for SessionUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        match parts.extensions.get::<Principal>() {
            Some(Principal { kind: "session", name, session_id: Some(id) }) => Ok(SessionUser { session_id: id.clone(), name: name.clone() }),
            Some(_) => Err(ApiError(StatusCode::FORBIDDEN, "この操作にはログインセッションが必要です".into())),
            None => Err(ApiError(StatusCode::UNAUTHORIZED, MSG_LOGIN_REQUIRED.into())),
        }
    }
}

fn session_cookie(token: &str, max_age: i64, secure: bool) -> HeaderValue {
    let mut c = format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}");
    if secure {
        c.push_str("; Secure");
    }
    HeaderValue::from_str(&c).expect("cookie is ASCII")
}

fn with_cookie(cookie: HeaderValue, body: serde_json::Value) -> Response {
    let mut res = Json(body).into_response();
    res.headers_mut().append(SET_COOKIE, cookie);
    res
}

fn ok_json() -> Response {
    Json(json!({"ok": true})).into_response()
}

fn err_resp(status: StatusCode, body: serde_json::Value) -> Response {
    (status, Json(body)).into_response()
}

async fn verify_stored_password(mgr: &AuthManager, password: String) -> bool {
    let Some(hash) = mgr.read_file(|f| f.password_hash.clone()) else { return false };
    tokio::task::spawn_blocking(move || verify_password(&password, &hash)).await.unwrap_or(false)
}

// --- status ----------------------------------------------------------------------------------

async fn status(State(mgr): State<Arc<AuthManager>>, client: Client, principal: Option<Extension<Principal>>) -> Json<serde_json::Value> {
    let configured = mgr.is_configured();
    let principal = principal.map(|e| e.0);
    let authenticated = matches!(principal.as_ref().map(|p| p.kind), Some("session" | "api_key"));
    let user = if authenticated { mgr.username() } else { None };
    let totp_enabled = mgr.read_file(|f| f.totp.enabled).unwrap_or(false);
    let principal_kind = match principal {
        Some(p) => Some(p.kind),
        None if !configured && client.local => Some("local"),
        None => None,
    };
    Json(json!({
        "configured": configured,
        "authenticated": authenticated,
        "user": user,
        "totp_enabled": totp_enabled,
        "local": client.local,
        "principal_kind": principal_kind,
    }))
}

// --- setup / login / logout ------------------------------------------------------------------

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

async fn setup(State(mgr): State<Arc<AuthManager>>, client: Client, ApiJson(req): ApiJson<Credentials>) -> Result<Response, ApiError> {
    mgr.reload_if_changed();
    if mgr.mode() != Mode::Unconfigured {
        return Err(ApiError(StatusCode::FORBIDDEN, "既に初期設定済みです".into()));
    }
    if !client.local {
        return Err(ApiError(StatusCode::FORBIDDEN, "初期設定はワークステーション上 (localhost) からのみ可能です".into()));
    }
    validate_username(&req.username)?;
    validate_new_password(&req.password)?;
    let password = req.password;
    let hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
    let file = AuthFile {
        username: req.username.clone(),
        password_hash: hash,
        totp: TotpState::default(),
        pending_totp_secret: None,
        api_keys: vec![],
        sessions: vec![],
    };
    {
        // Check-and-set under the write lock: concurrent setups cannot both win.
        let mut inner = mgr.inner.write();
        if !matches!(inner.store, Store::Unconfigured) {
            return Err(ApiError(StatusCode::FORBIDDEN, "既に初期設定済みです".into()));
        }
        write_auth_file(&mgr.path, &file).map_err(|e| {
            tracing::error!("auth.json write failed: {e:#}");
            ApiError::internal("認証情報を保存できませんでした")
        })?;
        inner.sig = file_sig(&mgr.path);
        inner.store = Store::Ready(Box::new(file));
    }
    tracing::info!("auth: initial account created");
    let (token, _) = mgr.create_session(&client.ip.to_string(), &client.user_agent)?;
    Ok(with_cookie(session_cookie(&token, COOKIE_MAX_AGE, client.https), json!({"ok": true, "user": req.username})))
}

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
    #[serde(default)]
    totp: Option<String>,
}

async fn login(State(mgr): State<Arc<AuthManager>>, client: Client, ApiJson(req): ApiJson<LoginReq>) -> Result<Response, ApiError> {
    mgr.reload_if_changed();
    match mgr.mode() {
        Mode::Corrupt => return Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, MSG_CORRUPT.into())),
        Mode::Unconfigured => return Err(ApiError(StatusCode::FORBIDDEN, MSG_UNCONFIGURED.into())),
        Mode::Configured => {}
    }
    mgr.rate_check(client.ip)?;
    if req.password.len() > MAX_PASSWORD_BYTES || req.username.len() > 256 {
        mgr.rate_fail(client.ip);
        return Err(ApiError(StatusCode::UNAUTHORIZED, MSG_BAD_LOGIN.into()));
    }
    let Some((real_user, real_hash)) = mgr.read_file(|f| (f.username.clone(), f.password_hash.clone())) else {
        return Err(ApiError(StatusCode::UNAUTHORIZED, MSG_BAD_LOGIN.into()));
    };
    let user_match = bool::from(real_user.as_bytes().ct_eq(req.username.as_bytes()));
    let password = req.password;
    // Always run a verification (dummy hash for unknown users) so timing does not reveal the username.
    let pw_ok = tokio::task::spawn_blocking(move || {
        let phc = if user_match { real_hash } else { dummy_hash().to_owned() };
        verify_password(&password, &phc)
    })
    .await
    .unwrap_or(false);
    if !(pw_ok && user_match) {
        mgr.rate_fail(client.ip);
        return Err(ApiError(StatusCode::UNAUTHORIZED, MSG_BAD_LOGIN.into()));
    }
    let totp_enabled = mgr.read_file(|f| f.totp.enabled).unwrap_or(false);
    if totp_enabled {
        let code = req.totp.as_deref().map(str::trim).unwrap_or("");
        if code.is_empty() {
            // Not counted as a failure: this is the normal first step of a two-step login.
            return Ok(err_resp(StatusCode::UNAUTHORIZED, json!({"detail": MSG_TOTP_REQUIRED, "totp_required": true})));
        }
        if !mgr.consume_totp(code, now_unix())? {
            mgr.rate_fail(client.ip);
            return Ok(err_resp(StatusCode::UNAUTHORIZED, json!({"detail": MSG_TOTP_WRONG, "totp_required": true})));
        }
    }
    mgr.rate_reset(client.ip);
    let (token, _) = mgr.create_session(&client.ip.to_string(), &client.user_agent)?;
    tracing::info!("auth: login from {}", client.ip);
    Ok(with_cookie(session_cookie(&token, COOKIE_MAX_AGE, client.https), json!({"ok": true, "user": real_user})))
}

async fn logout(State(mgr): State<Arc<AuthManager>>, client: Client, principal: Option<Extension<Principal>>) -> Result<Response, ApiError> {
    if let Some(Extension(Principal { session_id: Some(id), .. })) = principal {
        mgr.mutate(|f| f.sessions.retain(|s| s.id != id))?;
    }
    Ok(with_cookie(session_cookie("", 0, client.https), json!({"ok": true})))
}

// --- password --------------------------------------------------------------------------------

#[derive(Deserialize)]
struct PasswordReq {
    current: String,
    new: String,
}

async fn change_password(
    State(mgr): State<Arc<AuthManager>>,
    client: Client,
    user: SessionUser,
    ApiJson(req): ApiJson<PasswordReq>,
) -> Result<Response, ApiError> {
    mgr.rate_check(client.ip)?;
    validate_new_password(&req.new)?;
    if req.current.len() > MAX_PASSWORD_BYTES || !verify_stored_password(&mgr, req.current).await {
        mgr.rate_fail(client.ip);
        return Err(ApiError(StatusCode::UNAUTHORIZED, "現在のパスワードが違います".into()));
    }
    let new = req.new;
    let hash = tokio::task::spawn_blocking(move || hash_password(&new)).await.map_err(|e| ApiError::internal(e.to_string()))??;
    let keep = user.session_id.clone();
    mgr.mutate(|f| {
        f.password_hash = hash;
        f.sessions.retain(|s| s.id == keep);
    })?;
    tracing::info!("auth: password changed by {}", user.name);
    Ok(ok_json())
}

// --- TOTP ------------------------------------------------------------------------------------

async fn totp_setup(State(mgr): State<Arc<AuthManager>>, user: SessionUser) -> Result<Response, ApiError> {
    let secret = base32_encode(&random_bytes::<20>());
    let name = user.name;
    let s = secret.clone();
    mgr.mutate(|f| {
        if f.totp.enabled {
            return Err(ApiError(StatusCode::CONFLICT, "二段階認証は既に有効です。先に無効化してください".into()));
        }
        f.pending_totp_secret = Some(s);
        Ok(())
    })??;
    let url = format!("otpauth://totp/LLM%20Harness:{name}?secret={secret}&issuer=LLM%20Harness");
    Ok(Json(json!({"secret": secret, "otpauth_url": url})).into_response())
}

#[derive(Deserialize)]
struct CodeReq {
    code: String,
}

async fn totp_enable(State(mgr): State<Arc<AuthManager>>, client: Client, _user: SessionUser, ApiJson(req): ApiJson<CodeReq>) -> Result<Response, ApiError> {
    mgr.rate_check(client.ip)?;
    let now = now_unix();
    let result = mgr.mutate(|f| {
        let Some(pending) = f.pending_totp_secret.clone() else {
            return Err(ApiError::bad_request("先に二段階認証のセットアップを開始してください"));
        };
        let secret = base32_decode(&pending).ok_or_else(|| ApiError::internal("秘密鍵が不正です"))?;
        let Some(step) = verify_totp(&secret, &req.code, now, None) else {
            return Err(ApiError::bad_request("コードが違います"));
        };
        f.totp = TotpState { secret_b32: pending, enabled: true, last_step: Some(step) };
        f.pending_totp_secret = None;
        Ok(())
    })?;
    if result.is_err() {
        mgr.rate_fail(client.ip);
    }
    result?;
    Ok(ok_json())
}

#[derive(Deserialize)]
struct DisableTotpReq {
    password: String,
}

async fn totp_disable(
    State(mgr): State<Arc<AuthManager>>,
    client: Client,
    _user: SessionUser,
    ApiJson(req): ApiJson<DisableTotpReq>,
) -> Result<Response, ApiError> {
    mgr.rate_check(client.ip)?;
    if req.password.len() > MAX_PASSWORD_BYTES || !verify_stored_password(&mgr, req.password).await {
        mgr.rate_fail(client.ip);
        return Err(ApiError(StatusCode::UNAUTHORIZED, "パスワードが違います".into()));
    }
    mgr.mutate(|f| {
        f.totp = TotpState::default();
        f.pending_totp_secret = None;
    })?;
    Ok(ok_json())
}

// --- sessions --------------------------------------------------------------------------------

async fn list_sessions(State(mgr): State<Arc<AuthManager>>, user: SessionUser) -> Json<serde_json::Value> {
    let now = now_unix();
    let rows = mgr
        .read_file(|f| {
            let mut v: Vec<&SessionRec> = f.sessions.iter().filter(|s| s.is_live(now)).collect();
            v.sort_by_key(|s| std::cmp::Reverse(s.last_seen_at));
            v.into_iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "created_at": ts(s.created_at),
                        "last_seen_at": ts(s.last_seen_at),
                        "ip": s.ip,
                        "user_agent": s.user_agent,
                        "current": s.id == user.session_id,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Json(json!(rows))
}

async fn delete_session(State(mgr): State<Arc<AuthManager>>, client: Client, user: SessionUser, UrlPath(id): UrlPath<String>) -> Result<Response, ApiError> {
    let removed = mgr.mutate(|f| {
        let before = f.sessions.len();
        f.sessions.retain(|s| s.id != id);
        f.sessions.len() != before
    })?;
    if !removed {
        return Err(ApiError::not_found("セッションが見つかりません"));
    }
    if id == user.session_id {
        return Ok(with_cookie(session_cookie("", 0, client.https), json!({"ok": true})));
    }
    Ok(ok_json())
}

// --- API keys --------------------------------------------------------------------------------

#[derive(Deserialize)]
struct KeyReq {
    name: String,
    /// "openai" (default) | "anthropic" | "generic"
    #[serde(default)]
    kind: Option<String>,
}

async fn create_key(State(mgr): State<Arc<AuthManager>>, _user: SessionUser, ApiJson(req): ApiJson<KeyReq>) -> Result<Response, ApiError> {
    let name = req.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request("キー名は 1〜64 文字で指定してください"));
    }
    let kind = req.kind.as_deref().unwrap_or("openai").to_owned();
    let Some(key) = generate_key(&kind) else {
        return Err(ApiError::bad_request("kind は openai / anthropic / generic のいずれかです"));
    };
    let prefix_len = if kind == "generic" { 8 } else { key_prefix_of(&kind).map_or(8, str::len) + 4 };
    let rec = ApiKeyRec {
        id: random_hex(8),
        name: name.clone(),
        kind: kind.clone(),
        prefix: key.chars().take(prefix_len).collect(),
        hash: sha256_hex(key.as_bytes()),
        created_at: now_unix(),
        last_used_at: None,
    };
    let id = rec.id.clone();
    let prefix = rec.prefix.clone();
    let created_at = ts(rec.created_at);
    mgr.mutate(|f| {
        if f.api_keys.len() >= MAX_API_KEYS {
            return Err(ApiError::bad_request("API キーの上限に達しました"));
        }
        f.api_keys.push(rec);
        Ok(())
    })??;
    Ok(Json(json!({"id": id, "name": name, "kind": kind, "key": key, "prefix": prefix, "created_at": created_at})).into_response())
}

async fn list_keys(State(mgr): State<Arc<AuthManager>>, _user: SessionUser) -> Json<serde_json::Value> {
    let rows = mgr
        .read_file(|f| {
            f.api_keys
                .iter()
                .map(|k| {
                    json!({
                        "id": k.id,
                        "name": k.name,
                        "kind": k.kind,
                        "prefix": k.prefix,
                        "created_at": ts(k.created_at),
                        "last_used": k.last_used_at.map(ts),
                        "last_used_at": k.last_used_at.map(ts),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Json(json!(rows))
}

async fn delete_key(State(mgr): State<Arc<AuthManager>>, _user: SessionUser, UrlPath(id): UrlPath<String>) -> Result<Response, ApiError> {
    let removed = mgr.mutate(|f| {
        let before = f.api_keys.len();
        f.api_keys.retain(|k| k.id != id);
        f.api_keys.len() != before
    })?;
    if !removed {
        return Err(ApiError::not_found("API キーが見つかりません"));
    }
    Ok(ok_json())
}

// ---------------------------------------------------------------------------------------------
// tests

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderName;
    use axum::middleware::{from_fn, from_fn_with_state};
    use serde_json::Value;

    const PW: &str = "correct horse battery";

    struct Harness {
        base: String,
        mgr: Arc<AuthManager>,
        path: PathBuf,
        http: reqwest::Client,
        _dir: tempfile::TempDir,
    }

    async fn inject_peer(mut req: Request, next: Next) -> Response {
        let peer: SocketAddr = req
            .headers()
            .get("x-test-peer")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| "127.0.0.1:50000".parse().unwrap());
        req.extensions_mut().insert(ConnectInfo(peer));
        next.run(req).await
    }

    async fn spawn_with(dir: tempfile::TempDir, path: PathBuf) -> Harness {
        let mgr = AuthManager::load_from(path.clone());
        let app = Router::new()
            .route("/api/protected", get(|| async { "secret" }).post(|| async { "posted" }))
            .route("/v1/ping", get(|| async { "pong" }).post(|| async { "pong" }))
            .route("/v1/messages", post(|| async { "msg" }))
            .route("/api/system", get(|| async { "system" }))
            .route("/api/tunnel/start", post(|| async { "tunnel" }))
            .merge(routes::<Arc<AuthManager>>())
            .route_layer(from_fn_with_state(mgr.clone(), |State(m): State<Arc<AuthManager>>, req: Request, next: Next| async move {
                guard(&m, req, next).await
            }))
            .route("/api/health", get(|| async { "ok" }))
            .with_state(mgr.clone())
            .layer(from_fn(security_headers))
            .layer(from_fn(inject_peer));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Harness {
            base: format!("http://{addr}"),
            mgr,
            path,
            http: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap(),
            _dir: dir,
        }
    }

    async fn spawn() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        spawn_with(dir, path).await
    }

    struct Resp {
        status: u16,
        headers: reqwest::header::HeaderMap,
        body: Value,
    }

    impl Resp {
        fn cookie(&self) -> Option<String> {
            let c = self.headers.get("set-cookie")?.to_str().ok()?;
            Some(c.split(';').next()?.to_owned())
        }
    }

    impl Harness {
        async fn call(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Resp {
            let mut rb = self.http.request(method.parse().unwrap(), format!("{}{}", self.base, path));
            for (k, v) in headers {
                rb = rb.header(*k, *v);
            }
            if let Some(b) = body {
                rb = rb.json(&b);
            }
            let r = rb.send().await.unwrap();
            let status = r.status().as_u16();
            let headers = r.headers().clone();
            let text = r.text().await.unwrap();
            let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
            Resp { status, headers, body }
        }
        async fn setup(&self) -> String {
            let r = self.call("POST", "/api/auth/setup", &[], Some(json!({"username": "admin", "password": PW}))).await;
            assert_eq!(r.status, 200, "{:?}", r.body);
            r.cookie().unwrap()
        }
        async fn login(&self, extra: &[(&str, &str)], pw: &str, totp: Option<&str>) -> Resp {
            let mut b = json!({"username": "admin", "password": pw});
            if let Some(t) = totp {
                b["totp"] = json!(t);
            }
            self.call("POST", "/api/auth/login", extra, Some(b)).await
        }
    }

    // --- primitives ---

    #[test]
    fn totp_rfc6238_vector() {
        let secret = b"12345678901234567890";
        assert_eq!(totp_at(secret, 59 / 30), "287082");
        assert_eq!(totp_at(secret, 1111111109 / 30), "081804");
        assert_eq!(totp_at(secret, 1234567890 / 30), "005924");
        assert_eq!(verify_totp(secret, "287082", 59, None), Some(1));
        // +-1 step tolerance, but not beyond
        assert_eq!(verify_totp(secret, "287082", 59 + 30, None), Some(1));
        assert_eq!(verify_totp(secret, "287082", 59 + 90, None), None);
        // replay: step 1 already used
        assert_eq!(verify_totp(secret, "287082", 59, Some(1)), None);
        assert_eq!(verify_totp(secret, "28708", 59, None), None);
        assert_eq!(verify_totp(secret, "abcdef", 59, None), None);
    }

    #[test]
    fn base32_roundtrip() {
        assert_eq!(base32_encode(b"12345678901234567890"), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(base32_decode("gezd gnbv-GY3TQOJQGEZDGNBVGY3TQOJQ==").unwrap(), b"12345678901234567890");
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert!(base32_decode("not!valid").is_none());
        for n in 0..40usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(base32_decode(&base32_encode(&data)).unwrap(), data);
        }
    }

    #[test]
    fn locality_rules() {
        let uri: Uri = "/api/x".parse().unwrap();
        let lo: Option<SocketAddr> = Some("127.0.0.1:1234".parse().unwrap());
        let lo6: Option<SocketAddr> = Some("[::1]:1234".parse().unwrap());
        let remote: Option<SocketAddr> = Some("8.8.8.8:1234".parse().unwrap());
        let mut h = HeaderMap::new();
        h.insert("host", "localhost:8787".parse().unwrap());
        assert!(is_local_parts(lo, &h, &uri));
        assert!(is_local_parts(lo6, &h, &uri));
        assert!(!is_local_parts(remote, &h, &uri));
        assert!(!is_local_parts(None, &h, &uri));
        for name in [
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "cf-connecting-ip",
            "cf-ray",
            "x-real-ip",
            "ngrok-skip-browser-warning",
            "ngrok-trace-id",
        ] {
            let mut hh = h.clone();
            hh.insert(HeaderName::from_static(name), "1".parse().unwrap());
            assert!(!is_local_parts(lo, &hh, &uri), "{name} must defeat locality");
        }
        // DNS rebinding: loopback peer but attacker hostname
        let mut hh = HeaderMap::new();
        hh.insert("host", "evil.example.com:8787".parse().unwrap());
        assert!(!is_local_parts(lo, &hh, &uri));
        hh.insert("host", "127.0.0.1:8787".parse().unwrap());
        assert!(is_local_parts(lo, &hh, &uri));
        hh.insert("host", "[::1]:8787".parse().unwrap());
        assert!(is_local_parts(lo, &hh, &uri));
    }

    #[test]
    fn client_ip_priority() {
        let peer: Option<SocketAddr> = Some("127.0.0.1:1".parse().unwrap());
        let mut h = HeaderMap::new();
        assert_eq!(client_ip(peer, &h), "127.0.0.1".parse::<IpAddr>().unwrap());
        h.insert("x-real-ip", "3.3.3.3".parse().unwrap());
        assert_eq!(client_ip(peer, &h), "3.3.3.3".parse::<IpAddr>().unwrap());
        h.insert("x-forwarded-for", "2.2.2.2, 9.9.9.9".parse().unwrap());
        assert_eq!(client_ip(peer, &h), "2.2.2.2".parse::<IpAddr>().unwrap());
        h.insert("cf-connecting-ip", "1.1.1.1".parse().unwrap());
        assert_eq!(client_ip(peer, &h), "1.1.1.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn limiter_window() {
        let mut l = Limiter::default();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..5 {
            assert!(l.check(ip, t0).is_none());
            l.fail(ip, t0);
        }
        assert!(l.check(ip, t0).is_some());
        assert!(l.check("5.6.7.8".parse().unwrap(), t0).is_none());
        assert!(l.check(ip, t0 + RATE_WINDOW).is_none());
        l.reset(ip);
        // global cap across rotating IPs
        let mut l = Limiter::default();
        for i in 0..30u8 {
            l.fail(IpAddr::from([10, 0, 0, i]), t0);
        }
        assert!(l.check(IpAddr::from([10, 9, 9, 9]), t0).is_some());
    }

    // --- HTTP behaviour ---

    #[tokio::test]
    async fn unconfigured_local_allowed_remote_denied() {
        let h = spawn().await;
        assert!(!h.mgr.is_configured());
        let r = h.call("GET", "/api/protected", &[], None).await;
        assert_eq!(r.status, 200);
        // Tunnels connect from loopback but add forwarding headers
        for hdr in [("x-forwarded-for", "8.8.8.8"), ("cf-connecting-ip", "8.8.8.8"), ("ngrok-skip-browser-warning", "1"), ("cf-ray", "abc")] {
            let r = h.call("GET", "/api/protected", &[hdr], None).await;
            assert_eq!(r.status, 401, "{hdr:?}");
            assert_eq!(r.body["detail"], MSG_UNCONFIGURED);
        }
        let r = h.call("GET", "/api/protected", &[("x-test-peer", "8.8.8.8:4000")], None).await;
        assert_eq!(r.status, 401);
        let r = h.call("GET", "/api/protected", &[("host", "evil.example")], None).await;
        assert_eq!(r.status, 401, "rebinding host");
        // status is public
        let r = h.call("GET", "/api/auth/status", &[("x-test-peer", "8.8.8.8:4000")], None).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body["configured"], false);
        assert_eq!(r.body["local"], false);
        assert_eq!(r.body["authenticated"], false);
        let r = h.call("GET", "/api/auth/status", &[], None).await;
        assert_eq!(r.body["local"], true);
        assert_eq!(r.body["principal_kind"], "local");
    }

    #[tokio::test]
    async fn setup_refused_remotely_and_validates() {
        let h = spawn().await;
        let body = json!({"username": "admin", "password": PW});
        for hdr in [("x-test-peer", "8.8.8.8:4000"), ("x-forwarded-for", "8.8.8.8"), ("cf-connecting-ip", "1.1.1.1")] {
            let r = h.call("POST", "/api/auth/setup", &[hdr], Some(body.clone())).await;
            assert_eq!(r.status, 403, "{hdr:?}");
        }
        assert!(!h.mgr.is_configured());
        assert!(!h.path.exists());
        let r = h.call("POST", "/api/auth/setup", &[], Some(json!({"username": "admin", "password": "short"}))).await;
        assert_eq!(r.status, 400);
        let r = h.call("POST", "/api/auth/setup", &[], Some(json!({"username": "a b", "password": PW}))).await;
        assert_eq!(r.status, 400);
        let r = h.call("POST", "/api/auth/setup", &[], Some(json!({"username": "", "password": PW}))).await;
        assert_eq!(r.status, 400);
        let r = h.call("POST", "/api/auth/setup", &[], Some(json!({"nope": 1}))).await;
        assert!(r.status >= 400 && r.status < 500);
        assert!(r.body["detail"].is_string());

        let cookie = h.setup().await;
        assert!(h.mgr.is_configured());
        // second setup refused, even locally
        let r = h.call("POST", "/api/auth/setup", &[], Some(body)).await;
        assert_eq!(r.status, 403);
        // NO local bypass once configured
        let r = h.call("GET", "/api/protected", &[], None).await;
        assert_eq!(r.status, 401);
        assert_eq!(r.body["detail"], MSG_LOGIN_REQUIRED);
        let r = h.call("GET", "/api/protected", &[("cookie", &cookie)], None).await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn login_ok_wrong_password_and_cookie_attrs() {
        let h = spawn().await;
        h.setup().await;
        let r = h.login(&[], "wrong password here", None).await;
        assert_eq!(r.status, 401);
        assert_eq!(r.body["detail"], MSG_BAD_LOGIN);
        let r = h.call("POST", "/api/auth/login", &[], Some(json!({"username": "nobody", "password": PW}))).await;
        assert_eq!(r.status, 401);
        assert_eq!(r.body["detail"], MSG_BAD_LOGIN);

        let r = h.login(&[], PW, None).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.body, json!({"ok": true, "user": "admin"}));
        let c = r.headers.get("set-cookie").unwrap().to_str().unwrap().to_owned();
        assert!(c.starts_with("harness_session="));
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("Path=/") && c.contains("Max-Age=2592000"));
        assert!(!c.contains("Secure"));
        let token = r.cookie().unwrap();
        assert_eq!(token.len(), "harness_session=".len() + TOKEN_LEN);

        let r = h.login(&[("x-forwarded-proto", "https")], PW, None).await;
        assert!(r.headers.get("set-cookie").unwrap().to_str().unwrap().ends_with("; Secure"));
        let r = h.login(&[("cf-visitor", "{\"scheme\":\"https\"}")], PW, None).await;
        assert!(r.headers.get("set-cookie").unwrap().to_str().unwrap().ends_with("; Secure"));

        // security headers
        let r = h.call("GET", "/api/auth/status", &[], None).await;
        assert_eq!(r.headers["x-frame-options"], "DENY");
        assert_eq!(r.headers["x-content-type-options"], "nosniff");
        assert_eq!(r.headers["referrer-policy"], "no-referrer");
        assert_eq!(r.headers["permissions-policy"], "camera=(), microphone=(), geolocation=()");
        assert_eq!(r.headers["cache-control"], "no-store");
        let r = h.call("GET", "/api/health", &[], None).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.headers["cache-control"], "no-store");
    }

    #[tokio::test]
    async fn rate_limit_per_ip() {
        let h = spawn().await;
        h.setup().await;
        for _ in 0..5 {
            let r = h.login(&[("cf-connecting-ip", "6.6.6.6")], "wrong password here", None).await;
            assert_eq!(r.status, 401);
        }
        // 6th attempt is refused even with the right password
        let r = h.login(&[("cf-connecting-ip", "6.6.6.6")], PW, None).await;
        assert_eq!(r.status, 429);
        assert!(r.body["detail"].as_str().unwrap().starts_with("ログイン試行が多すぎます。"));
        assert!(r.body["detail"].as_str().unwrap().ends_with("分後に再試行してください"));
        // another IP is unaffected, and success resets only that IP
        let r = h.login(&[("cf-connecting-ip", "7.7.7.7")], PW, None).await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn rate_limit_global_cap_with_ip_rotation() {
        let h = spawn().await;
        h.setup().await;
        for i in 0..30 {
            let ip = format!("20.0.0.{i}");
            let r = h.login(&[("cf-connecting-ip", ip.as_str())], "wrong password here", None).await;
            assert_eq!(r.status, 401);
        }
        let r = h.login(&[("cf-connecting-ip", "21.0.0.1")], PW, None).await;
        assert_eq!(r.status, 429);
    }

    #[tokio::test]
    async fn cookie_auth_and_csrf() {
        let h = spawn().await;
        let cookie = h.setup().await;
        let r = h.call("GET", "/api/protected", &[("cookie", &cookie)], None).await;
        assert_eq!(r.status, 200);
        // unsafe method without the CSRF header
        let r = h.call("POST", "/api/protected", &[("cookie", &cookie)], None).await;
        assert_eq!(r.status, 403);
        let r = h.call("POST", "/api/protected", &[("cookie", &cookie), ("x-harness-csrf", "0")], None).await;
        assert_eq!(r.status, 403);
        let r = h.call("POST", "/api/protected", &[("cookie", &cookie), ("x-harness-csrf", "1")], None).await;
        assert_eq!(r.status, 200);
        // garbage cookies
        for bad in ["harness_session=abc", "harness_session=", &format!("harness_session={}", "A".repeat(TOKEN_LEN))] {
            let r = h.call("GET", "/api/protected", &[("cookie", bad)], None).await;
            assert_eq!(r.status, 401);
        }
        // invalid cookie + POST is 401 (not authenticated), not 403
        let r = h.call("POST", "/api/protected", &[("cookie", "harness_session=zzz")], None).await;
        assert_eq!(r.status, 401);
        // cookie among others
        let both = format!("a=b; {cookie}; c=d");
        let r = h.call("GET", "/api/protected", &[("cookie", both.as_str())], None).await;
        assert_eq!(r.status, 200);
        // management endpoints enforce CSRF as well
        let r = h.call("POST", "/api/auth/keys", &[("cookie", &cookie)], Some(json!({"name": "x"}))).await;
        assert_eq!(r.status, 403);
        // status reflects the session
        let r = h.call("GET", "/api/auth/status", &[("cookie", &cookie)], None).await;
        assert_eq!(r.body["authenticated"], true);
        assert_eq!(r.body["user"], "admin");
        assert_eq!(r.body["principal_kind"], "session");
        assert_eq!(r.body["configured"], true);
    }

    #[test]
    fn key_kinds_generate_and_validate() {
        let o = generate_key("openai").unwrap();
        let a = generate_key("anthropic").unwrap();
        let g = generate_key("generic").unwrap();
        assert!(o.starts_with("sk-lh-") && o.len() == 6 + TOKEN_LEN);
        assert!(a.starts_with("sk-ant-lh-") && a.len() == 10 + TOKEN_LEN);
        assert!(g.starts_with("lh_") && g.len() == 3 + TOKEN_LEN);
        assert!(generate_key("bogus").is_none());
        assert_ne!(generate_key("openai"), Some(o.clone()));
        for k in [&o, &a, &g] {
            assert!(key_has_valid_shape(k));
        }
        assert!(!key_has_valid_shape("sk-lh-short") && !key_has_valid_shape("sk-ant-lh-") && !key_has_valid_shape(&format!("sk-{}", "a".repeat(TOKEN_LEN))));
    }

    #[tokio::test]
    async fn api_keys_kinds_and_header_styles() {
        let h = spawn().await;
        let cookie = h.setup().await;
        let auth = [("cookie", cookie.as_str()), ("x-harness-csrf", "1")];
        let mut keys = Vec::new();
        for (kind, prefix) in [(None, "sk-lh-"), (Some("openai"), "sk-lh-"), (Some("anthropic"), "sk-ant-lh-"), (Some("generic"), "lh_")] {
            let body = match kind {
                Some(k) => json!({"name": "k", "kind": k}),
                None => json!({"name": "k"}),
            };
            let r = h.call("POST", "/api/auth/keys", &auth, Some(body)).await;
            assert_eq!(r.status, 200, "{:?}", r.body);
            let key = r.body["key"].as_str().unwrap().to_owned();
            assert!(key.starts_with(prefix));
            assert_eq!(r.body["kind"], kind.unwrap_or("openai"));
            assert!(key.starts_with(r.body["prefix"].as_str().unwrap()));
            assert!(r.body["created_at"].is_string() && r.body["id"].is_string());
            // only hashes are stored
            let stored = std::fs::read_to_string(&h.path).unwrap();
            assert!(!stored.contains(&key) && stored.contains(&sha256_hex(key.as_bytes())));
            keys.push(key);
        }
        let r = h.call("POST", "/api/auth/keys", &auth, Some(json!({"name": "k", "kind": "bogus"}))).await;
        assert_eq!(r.status, 400);

        for key in &keys {
            // Bearer and x-api-key both work on /v1, without CSRF header, for any method
            let bearer = format!("Bearer {key}");
            assert_eq!(h.call("POST", "/v1/ping", &[("authorization", &bearer)], None).await.status, 200);
            assert_eq!(h.call("POST", "/v1/messages", &[("x-api-key", key.as_str()), ("anthropic-version", "2023-06-01")], None).await.status, 200);
            assert_eq!(h.call("GET", "/v1/ping", &[("x-api-key", key.as_str())], None).await.status, 200);
        }
        let r = h.call("GET", "/api/auth/keys", &auth, None).await;
        let rows = r.body.as_array().unwrap();
        assert_eq!(rows.len(), 4);
        let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["openai", "openai", "anthropic", "generic"]);
        assert!(rows.iter().all(|r| r["last_used"].is_string() && r.get("key").is_none() && r.get("hash").is_none() && r["prefix"].is_string()));
    }

    #[tokio::test]
    async fn legacy_keys_without_kind_still_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let h = spawn_with(dir, path.clone()).await;
        let cookie = h.setup().await;
        let key = format!("lh_{}", random_token());
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["api_keys"] = json!([{"id": "old1", "name": "legacy", "prefix": &key[..8], "hash": sha256_hex(key.as_bytes()), "created_at": 1700000000}]);
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        let r = h.call("GET", "/v1/ping", &[("authorization", &format!("Bearer {key}"))], None).await;
        assert_eq!(r.status, 200);
        let r = h.call("GET", "/api/auth/keys", &[("cookie", cookie.as_str())], None).await;
        assert_eq!(r.body[0]["kind"], "generic");
    }

    #[tokio::test]
    async fn api_keys_only_reach_v1() {
        let h = spawn().await;
        let cookie = h.setup().await;
        let auth = [("cookie", cookie.as_str()), ("x-harness-csrf", "1")];
        let r = h.call("POST", "/api/auth/keys", &auth, Some(json!({"name": "script", "kind": "anthropic"}))).await;
        let key = r.body["key"].as_str().unwrap().to_owned();
        let id = r.body["id"].as_str().unwrap().to_owned();
        let bearer = format!("Bearer {key}");
        for hdr in [("authorization", bearer.as_str()), ("x-api-key", key.as_str())] {
            for (m, p) in [
                ("GET", "/api/system"),
                ("POST", "/api/tunnel/start"),
                ("GET", "/api/protected"),
                ("GET", "/api/auth/keys"),
                ("POST", "/api/auth/keys"),
                ("DELETE", "/api/auth/keys/x"),
                ("POST", "/api/auth/password"),
                ("GET", "/api/auth/sessions"),
            ] {
                let r = h.call(m, p, &[hdr], Some(json!({"name": "evil"}))).await;
                assert_eq!(r.status, 403, "{m} {p}");
            }
        }
        // the session still reaches them
        assert_eq!(h.call("GET", "/api/system", &auth, None).await.status, 200);
        // status is public: a key is simply not a login there
        let r = h.call("GET", "/api/auth/status", &[("authorization", &bearer)], None).await;
        assert_eq!(r.body["authenticated"], false);

        // wrong keys: 401 in the error shape of the API; Anthropic shape for /v1/messages
        let wrong = format!("sk-ant-lh-{}", "B".repeat(TOKEN_LEN));
        let r = h.call("GET", "/v1/ping", &[("x-api-key", &wrong)], None).await;
        assert_eq!(r.status, 401);
        assert_eq!(r.body["error"]["code"], "authentication_error");
        let r = h.call("POST", "/v1/messages", &[("x-api-key", &wrong), ("anthropic-version", "2023-06-01")], None).await;
        assert_eq!(r.status, 401);
        assert_eq!(r.body["type"], "error");
        assert_eq!(r.body["error"]["type"], "authentication_error");
        assert_eq!(h.call("GET", "/v1/ping", &[], None).await.status, 401);

        // revoke
        assert_eq!(h.call("DELETE", &format!("/api/auth/keys/{id}"), &auth, None).await.status, 200);
        assert_eq!(h.call("GET", "/v1/ping", &[("x-api-key", &key)], None).await.status, 401);
        assert_eq!(h.call("DELETE", &format!("/api/auth/keys/{id}"), &auth, None).await.status, 404);
    }

    #[tokio::test]
    async fn failed_key_attempts_are_rate_limited() {
        let h = spawn().await;
        h.setup().await;
        let wrong = format!("sk-lh-{}", "C".repeat(TOKEN_LEN));
        for _ in 0..RATE_MAX_PER_IP {
            assert_eq!(h.call("GET", "/v1/ping", &[("x-api-key", &wrong)], None).await.status, 401);
        }
        let r = h.call("GET", "/v1/ping", &[("x-api-key", &wrong)], None).await;
        assert_eq!(r.status, 429);
        // login attempts use their own budget
        assert_eq!(h.login(&[], PW, None).await.status, 200);
        // another IP is unaffected
        let r = h.call("GET", "/v1/ping", &[("x-api-key", &wrong), ("x-test-peer", "198.51.100.7:1")], None).await;
        assert_eq!(r.status, 401);
    }

    #[tokio::test]
    async fn revoked_session_is_rejected() {
        let h = spawn().await;
        let c1 = h.setup().await;
        let c2 = h.login(&[], PW, None).await.cookie().unwrap();
        let a1 = [("cookie", c1.as_str()), ("x-harness-csrf", "1")];
        let r = h.call("GET", "/api/auth/sessions", &a1, None).await;
        assert_eq!(r.status, 200);
        let rows = r.body.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().filter(|s| s["current"] == true).count(), 1);
        let other = rows.iter().find(|s| s["current"] == false).unwrap()["id"].as_str().unwrap().to_owned();
        for k in ["id", "created_at", "last_seen_at", "ip", "user_agent", "current"] {
            assert!(rows[0].get(k).is_some(), "{k}");
        }
        // c2 works, then gets revoked through the sessions API
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &c2)], None).await.status, 200);
        let r = h.call("DELETE", &format!("/api/auth/sessions/{other}"), &a1, None).await;
        assert_eq!(r.status, 200);
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &c2)], None).await.status, 401);
        // logout kills c1 and expires the cookie
        let r = h.call("POST", "/api/auth/logout", &a1, None).await;
        assert_eq!(r.status, 200);
        assert!(r.headers.get("set-cookie").unwrap().to_str().unwrap().contains("Max-Age=0"));
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &c1)], None).await.status, 401);
    }

    fn write_session_file(path: &Path, created: i64, last_seen: i64) -> String {
        let token = random_token();
        let file = AuthFile {
            username: "admin".into(),
            password_hash: hash_password(PW).unwrap(),
            totp: TotpState::default(),
            pending_totp_secret: None,
            api_keys: vec![],
            sessions: vec![SessionRec {
                id: "sid".into(),
                token_hash: sha256_hex(token.as_bytes()),
                created_at: created,
                last_seen_at: last_seen,
                ip: "1.1.1.1".into(),
                user_agent: "t".into(),
            }],
        };
        write_auth_file(path, &file).unwrap();
        format!("harness_session={token}")
    }

    #[tokio::test]
    async fn expired_sessions_rejected() {
        let day = 24 * 3600;
        let now = now_unix();
        // live control
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let c = write_session_file(&path, now - 2 * day, now - day);
        let h = spawn_with(dir, path).await;
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &c)], None).await.status, 200);
        // idle > 7 days
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let c = write_session_file(&path, now - 9 * day, now - 8 * day);
        let h = spawn_with(dir, path).await;
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &c)], None).await.status, 401);
        // absolute > 30 days even though recently active
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let c = write_session_file(&path, now - 31 * day, now - 60);
        let h = spawn_with(dir, path).await;
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &c)], None).await.status, 401);
        // session that expires while the server is running
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let h = spawn_with(dir, path).await;
        let cookie = h.setup().await;
        let token = cookie.strip_prefix("harness_session=").unwrap().to_owned();
        assert!(h.mgr.session_principal(&token, now_unix()).is_some());
        assert!(h.mgr.session_principal(&token, now_unix() + 8 * day).is_none());
        assert!(h.mgr.session_principal(&token, now_unix() + 31 * day).is_none());
    }

    #[tokio::test]
    async fn sliding_expiry_debounced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let now = now_unix();
        let c = write_session_file(&path, now - 3600, now - 30);
        let h = spawn_with(dir, path).await;
        let token = c.strip_prefix("harness_session=").unwrap();
        let before = h.mgr.read_file(|f| f.sessions[0].last_seen_at).unwrap();
        h.mgr.session_principal(token, now).unwrap();
        assert_eq!(h.mgr.read_file(|f| f.sessions[0].last_seen_at).unwrap(), before, "within a minute: no update");
        h.mgr.session_principal(token, now + 3600).unwrap();
        assert_eq!(h.mgr.read_file(|f| f.sessions[0].last_seen_at).unwrap(), now + 3600);
        let disk: Value = serde_json::from_slice(&std::fs::read(&h.path).unwrap()).unwrap();
        assert_eq!(disk["sessions"][0]["last_seen_at"], now + 3600);
    }

    #[tokio::test]
    async fn totp_flow_and_replay() {
        let h = spawn().await;
        let cookie = h.setup().await;
        let a = [("cookie", cookie.as_str()), ("x-harness-csrf", "1")];
        let r = h.call("POST", "/api/auth/totp/setup", &a, None).await;
        assert_eq!(r.status, 200);
        let secret = r.body["secret"].as_str().unwrap().to_owned();
        assert_eq!(r.body["otpauth_url"], format!("otpauth://totp/LLM%20Harness:admin?secret={secret}&issuer=LLM%20Harness"));
        let key = base32_decode(&secret).unwrap();
        assert_eq!(key.len(), 20);

        // wrong enable code
        let r = h.call("POST", "/api/auth/totp/enable", &a, Some(json!({"code": "000000"}))).await;
        assert!(r.status == 400, "{:?}", r.body);
        let step = now_unix() as u64 / 30;
        let r = h.call("POST", "/api/auth/totp/enable", &a, Some(json!({"code": totp_at(&key, step)}))).await;
        assert_eq!(r.status, 200, "{:?}", r.body);
        let r = h.call("GET", "/api/auth/status", &[], None).await;
        assert_eq!(r.body["totp_enabled"], true);

        // password only: totp_required, no cookie
        let r = h.login(&[], PW, None).await;
        assert_eq!(r.status, 401);
        assert_eq!(r.body["totp_required"], true);
        assert!(r.headers.get("set-cookie").is_none());
        // wrong code
        let r = h.login(&[], PW, Some("123456")).await;
        assert_eq!(r.status, 401);
        assert_eq!(r.body, json!({"detail": MSG_TOTP_WRONG, "totp_required": true}));
        // the enable code (current step) was consumed: replay refused
        let r = h.login(&[], PW, Some(&totp_at(&key, step))).await;
        assert_eq!(r.status, 401, "replay of the enabling code");
        // next step's code is within tolerance and accepted once
        let code = totp_at(&key, step + 1);
        let r = h.login(&[], PW, Some(&code)).await;
        assert_eq!(r.status, 200, "{:?}", r.body);
        let r = h.login(&[], PW, Some(&code)).await;
        assert_eq!(r.status, 401, "replay of an accepted code");
        // wrong password with right code never logs in
        let r = h.login(&[], "wrong password here", Some(&totp_at(&key, step + 1))).await;
        assert_eq!(r.status, 401);

        // replay protection survives restart (persisted)
        let again = AuthManager::load_from(h.path.clone());
        assert!(!again.consume_totp(&code, now_unix()).unwrap());

        // disable requires the password
        let r = h.call("POST", "/api/auth/totp/disable", &a, Some(json!({"password": "wrong password here"}))).await;
        assert_eq!(r.status, 401);
        let r = h.call("POST", "/api/auth/totp/disable", &a, Some(json!({"password": PW}))).await;
        assert_eq!(r.status, 200);
        assert_eq!(h.login(&[], PW, None).await.status, 200);
    }

    #[tokio::test]
    async fn password_change_revokes_other_sessions() {
        let h = spawn().await;
        let c1 = h.setup().await;
        let c2 = h.login(&[], PW, None).await.cookie().unwrap();
        let a = [("cookie", c1.as_str()), ("x-harness-csrf", "1")];
        let r = h.call("POST", "/api/auth/password", &a, Some(json!({"current": "nope nope nope nope", "new": "brand new password!"}))).await;
        assert_eq!(r.status, 401);
        let r = h.call("POST", "/api/auth/password", &a, Some(json!({"current": PW, "new": "short"}))).await;
        assert_eq!(r.status, 400);
        let r = h.call("POST", "/api/auth/password", &a, Some(json!({"current": PW, "new": "brand new password!"}))).await;
        assert_eq!(r.status, 200);
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &c1)], None).await.status, 200);
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &c2)], None).await.status, 401);
        assert_eq!(h.login(&[], PW, None).await.status, 401);
        assert_eq!(h.login(&[], "brand new password!", None).await.status, 200);
    }

    #[tokio::test]
    async fn corrupt_file_fails_closed() {
        for content in ["{ not json", "", "{}", r#"{"username":"admin","password_hash":"garbage"}"#, r#"{"username":"","password_hash":""}"#] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("auth.json");
            std::fs::write(&path, content).unwrap();
            let h = spawn_with(dir, path.clone()).await;
            assert!(h.mgr.is_configured(), "{content:?}");
            // local, direct, no headers: still denied
            let r = h.call("GET", "/api/protected", &[], None).await;
            assert_eq!(r.status, 401, "{content:?}");
            assert_eq!(r.body["detail"], MSG_CORRUPT);
            let r = h.call("POST", "/api/auth/setup", &[], Some(json!({"username": "admin", "password": PW}))).await;
            assert_eq!(r.status, 403);
            let r = h.login(&[], PW, None).await;
            assert!(r.status >= 400 && r.headers.get("set-cookie").is_none());
            let r = h.call("GET", "/api/auth/status", &[], None).await;
            assert_eq!(r.body["configured"], true);
            assert_eq!(r.body["authenticated"], false);
            // untouched on disk
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
            // CLI recovery works and the running server picks it up
            cli_set_password_at(&path, "admin", PW).unwrap();
            let r = h.login(&[], PW, None).await;
            assert_eq!(r.status, 200, "{content:?} {:?}", r.body);
        }
    }

    #[tokio::test]
    async fn deleting_file_does_not_reopen_panel() {
        let h = spawn().await;
        h.setup().await;
        std::fs::remove_file(&h.path).unwrap();
        let r = h.call("GET", "/api/protected", &[], None).await;
        assert_eq!(r.status, 401);
    }

    #[tokio::test]
    async fn cli_reset_applies_to_running_server_and_revokes_sessions() {
        let h = spawn().await;
        let cookie = h.setup().await;
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &cookie)], None).await.status, 200);
        cli_set_password_at(&h.path, "admin", "a totally new password").unwrap();
        assert_eq!(h.call("GET", "/api/protected", &[("cookie", &cookie)], None).await.status, 401);
        assert_eq!(h.login(&[], PW, None).await.status, 401);
        assert_eq!(h.login(&[], "a totally new password", None).await.status, 200);
        // validation
        assert!(cli_set_password_at(&h.path, "admin", "short").is_err());
        assert!(cli_set_password_at(&h.path, "bad name", PW).is_err());
    }

    #[tokio::test]
    async fn cli_disable_totp_works() {
        let h = spawn().await;
        let cookie = h.setup().await;
        let a = [("cookie", cookie.as_str()), ("x-harness-csrf", "1")];
        let secret = h.call("POST", "/api/auth/totp/setup", &a, None).await.body["secret"].as_str().unwrap().to_owned();
        let key = base32_decode(&secret).unwrap();
        let step = now_unix() as u64 / 30;
        assert_eq!(h.call("POST", "/api/auth/totp/enable", &a, Some(json!({"code": totp_at(&key, step)}))).await.status, 200);
        assert_eq!(h.login(&[], PW, None).await.status, 401);
        cli_disable_totp_at(&h.path).unwrap();
        assert_eq!(h.login(&[], PW, None).await.status, 200);
    }

    #[tokio::test]
    async fn file_is_private_and_holds_no_secrets() {
        let h = spawn().await;
        let cookie = h.setup().await;
        let stored = std::fs::read_to_string(&h.path).unwrap();
        let token = cookie.strip_prefix("harness_session=").unwrap();
        assert!(!stored.contains(token));
        assert!(!stored.contains(PW));
        assert!(stored.contains("$argon2id$"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&h.path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // survives a restart
        let again = AuthManager::load_from(h.path.clone());
        assert!(again.is_configured());
        assert!(again.session_principal(token, now_unix()).is_some());
    }

    #[tokio::test]
    async fn malformed_bodies_use_detail_shape() {
        let h = spawn().await;
        h.setup().await;
        let r = h.http.post(format!("{}/api/auth/login", h.base)).header("content-type", "text/plain").body("x").send().await.unwrap();
        assert!(r.status().is_client_error());
        let v: Value = r.json().await.unwrap();
        assert!(v["detail"].is_string());
        let r = h.call("POST", "/api/auth/login", &[], Some(json!({"username": "admin"}))).await;
        assert!(r.status >= 400 && r.body["detail"].is_string());
    }
}
