//! Remote administration API (`/api/admin/*`).
//!
//! The workstation runs the harness as a pure "processing unit": everything must be operable
//! from an authenticated browser through the tunnel. This module therefore exposes
//! configuration, environment, diagnostics, logs and a small set of *named* maintenance actions.
//!
//! Security decisions (deliberate, do not relax):
//! * **Session only.** Every route sits behind [`session_only`] and every handler takes the
//!   [`Admin`] extractor: only an authenticated *browser session* may call them. API keys
//!   (`Authorization: Bearer lh_...`) and the unauthenticated "local" setup principal get 403.
//! * **No arbitrary shell/exec endpoint and no arbitrary file read/write endpoint exist, on
//!   purpose.** A stolen browser session must not become remote code execution or a file
//!   exfiltration primitive. Actions come from a strict allowlist ([`ACTIONS`]); log sources
//!   from a fixed allowlist ([`Source`]); file-ish inputs (config history versions, job ids) are
//!   validated against tight patterns and never joined into paths unchecked.
//! * **Audit.** Every mutating call appends a JSONL record to `HARNESS_HOME/audit.log`
//!   (`ts, ip, user, action, summary`). Summaries only ever name *keys*, never values, and are
//!   additionally scrubbed of any known secret. The log rotates at ~5 MiB.
//! * **Secrets are masked** on read (`hf_token`, env vars whose name contains TOKEN/KEY/SECRET)
//!   and a masked value sent back unchanged never overwrites the stored secret.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime};

use axum::extract::{FromRequestParts, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::auth::{Client, Principal};
use crate::config::{expand, Settings, HARNESS_HOME, REPO_ROOT};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

// ═════════════════════════════════════════════════════════════════════ auth gate
const MSG_SESSION_REQUIRED: &str = "管理 API にはログインセッションが必要です (API キーでは使えません)";

/// Route-level gate: only authenticated browser sessions pass.
pub(crate) async fn session_only(req: Request, next: Next) -> Response {
    match req.extensions().get::<Principal>() {
        Some(Principal { kind: "session", .. }) => next.run(req).await,
        Some(_) => ApiError(StatusCode::FORBIDDEN, MSG_SESSION_REQUIRED.into()).into_response(),
        None => ApiError(StatusCode::UNAUTHORIZED, "ログインが必要です".into()).into_response(),
    }
}

/// The calling admin (second line of defence behind [`session_only`]).
pub(crate) struct Admin {
    user: String,
    ip: String,
}

impl<S: Send + Sync> FromRequestParts<S> for Admin {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let user = match parts.extensions.get::<Principal>() {
            Some(Principal { kind: "session", name, .. }) => name.clone(),
            Some(_) => return Err(ApiError(StatusCode::FORBIDDEN, MSG_SESSION_REQUIRED.into())),
            None => return Err(ApiError(StatusCode::UNAUTHORIZED, "ログインが必要です".into())),
        };
        let client = match Client::from_request_parts(parts, state).await {
            Ok(c) => c,
            Err(e) => match e {},
        };
        Ok(Admin { user, ip: client.ip.to_string() })
    }
}

// ═════════════════════════════════════════════════════════════════════ audit log
const AUDIT_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

/// Single line, no control characters, bounded; known secrets replaced.
fn scrub(text: &str, secrets: &[String]) -> String {
    let mut s: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(400).collect();
    for sec in secrets.iter().filter(|x| x.len() >= 4) {
        s = s.replace(sec.as_str(), "***");
    }
    s
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    pub fn append(&self, ip: &str, user: &str, action: &str, summary: &str, secrets: &[String]) {
        let _g = self.lock.lock();
        let rec = json!({
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "ip": scrub(ip, secrets),
            "user": scrub(user, secrets),
            "action": scrub(action, secrets),
            "summary": scrub(summary, secrets),
        });
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= AUDIT_ROTATE_BYTES) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&self.path) {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{rec}") {
                    tracing::warn!("audit log write failed: {e}");
                }
            }
            Err(e) => tracing::warn!("audit log open failed: {e}"),
        }
    }

    /// Newest first.
    pub fn read(&self, limit: usize) -> Vec<Value> {
        let _g = self.lock.lock();
        let mut out: Vec<Value> = vec![];
        for p in [self.path.clone(), self.path.with_extension("log.1")] {
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            for line in text.lines().rev() {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    out.push(v);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }
}

static AUDIT: LazyLock<AuditLog> = LazyLock::new(|| AuditLog::new(HARNESS_HOME.join("audit.log")));

/// Secret values currently stored, used to scrub audit summaries defensively.
fn known_secrets(s: &Settings) -> Vec<String> {
    let mut v: Vec<String> = vec![s.hf_token.clone()];
    v.extend(s.extra_env.iter().filter(|(k, _)| is_secret_name(k)).map(|(_, val)| val.clone()));
    v.retain(|x| !x.is_empty());
    v
}

/// Append one audit record for a mutating call (`summary` names keys or ids, never secret values).
pub(crate) fn audit(st: &AppState, who: &Admin, action: &str, summary: &str) {
    AUDIT.append(&who.ip, &who.user, action, summary, &known_secrets(&st.settings.get()));
}

// ═════════════════════════════════════════════════════════════════════ log ring buffer
const RING_CAP: usize = 2000;
static RING: LazyLock<Mutex<VecDeque<String>>> = LazyLock::new(|| Mutex::new(VecDeque::with_capacity(RING_CAP)));

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: String,
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push_str(&format!(" {}={:?}", field.name(), value));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push_str(&format!(" {}={}", field.name(), value));
        }
    }
}

/// `tracing_subscriber` layer keeping the last [`RING_CAP`] formatted lines in memory so the
/// `harness` log source works even when the binary does not write a log file.
pub struct RingLayer;

pub fn ring_layer() -> RingLayer {
    LazyLock::force(&START);
    RingLayer
}

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut c = FieldCollector::default();
        event.record(&mut c);
        let meta = event.metadata();
        let mut line = format!("{} {:>5} {}: {}{}", Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true), meta.level(), meta.target(), c.message, c.fields);
        if line.len() > 2000 {
            line = line.chars().take(2000).collect();
        }
        let mut ring = RING.lock();
        if ring.len() >= RING_CAP {
            ring.pop_front();
        }
        ring.push_back(line);
    }
}

fn ring_tail(n: usize) -> Vec<String> {
    let ring = RING.lock();
    ring.iter().skip(ring.len().saturating_sub(n)).cloned().collect()
}

// ═════════════════════════════════════════════════════════════════════ secrets & masking
pub fn is_secret_name(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    ["TOKEN", "KEY", "SECRET", "PASSWORD"].iter().any(|k| u.contains(k))
}

fn mask_secret(s: &str) -> String {
    let n = s.chars().count();
    if n == 0 {
        String::new()
    } else if n >= 12 {
        format!("****{}", s.chars().skip(n - 4).collect::<String>())
    } else {
        "****".into()
    }
}

fn is_masked(s: &str) -> bool {
    s.starts_with("****")
}

fn masked_env(env: &BTreeMap<String, String>) -> Value {
    Value::Object(env.iter().map(|(k, v)| (k.clone(), json!(if is_secret_name(k) { mask_secret(v) } else { v.clone() }))).collect())
}

fn settings_value(s: &Settings) -> Value {
    serde_json::to_value(s).unwrap_or(Value::Null)
}

/// Full settings as shown to the browser: secrets masked.
pub fn masked_settings(s: &Settings) -> Value {
    let mut v = settings_value(s);
    v["hf_token"] = json!(mask_secret(&s.hf_token));
    v["extra_env"] = masked_env(&s.extra_env);
    v
}

// ═════════════════════════════════════════════════════════════════════ env validation
pub const ENV_DENY: &[&str] = &["LD_PRELOAD", "LD_AUDIT"];
const MAX_ENV_ENTRIES: usize = 200;

pub fn validate_env_name(name: &str) -> Result<(), String> {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 128 {
        return Err(format!("環境変数名の長さが不正です: {name:?}"));
    }
    let first_ok = b[0].is_ascii_uppercase() || b[0] == b'_';
    if !first_ok || !b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_') {
        return Err(format!("環境変数名は [A-Z_][A-Z0-9_]* のみ使えます: {name:?}"));
    }
    if ENV_DENY.contains(&name) {
        return Err(format!("{name} は設定できません (禁止リスト)"));
    }
    Ok(())
}

/// Build the replacement env map. A masked value for an existing secret key keeps the stored value.
fn build_env(input: &Map<String, Value>, existing: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, ApiError> {
    if input.len() > MAX_ENV_ENTRIES {
        return Err(ApiError::bad_request("環境変数が多すぎます"));
    }
    let mut out = BTreeMap::new();
    for (k, v) in input {
        validate_env_name(k).map_err(ApiError::bad_request)?;
        let Value::String(val) = v else {
            return Err(ApiError::bad_request(format!("{k} の値は文字列で指定してください")));
        };
        if val.len() > 8192 || val.contains('\0') {
            return Err(ApiError::bad_request(format!("{k} の値が不正です")));
        }
        if is_secret_name(k) && is_masked(val) {
            match existing.get(k) {
                Some(old) => {
                    out.insert(k.clone(), old.clone());
                    continue;
                }
                None => return Err(ApiError::bad_request(format!("{k}: マスクされた値は新しい値として使えません"))),
            }
        }
        out.insert(k.clone(), val.clone());
    }
    Ok(out)
}

// ═════════════════════════════════════════════════════════════════════ settings validation
fn check_path(label: &str, raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() || raw.len() > 4096 || raw.contains('\0') {
        return Err(format!("{label}: パスが不正です"));
    }
    let p = expand(raw);
    if !p.is_absolute() {
        return Err(format!("{label}: 絶対パス (または ~/ 始まり) で指定してください"));
    }
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("{label}: '..' は使えません"));
    }
    if p.exists() {
        if !p.is_dir() {
            return Err(format!("{label}: ディレクトリではありません ({})", p.display()));
        }
        return Ok(());
    }
    let Some(anc) = p.ancestors().skip(1).find(|a| a.exists()) else {
        return Err(format!("{label}: 親ディレクトリが存在しません"));
    };
    if !anc.is_dir() {
        return Err(format!("{label}: 作成できません ({} はディレクトリではありません)", anc.display()));
    }
    let c = std::ffi::CString::new(anc.as_os_str().as_encoded_bytes()).map_err(|_| format!("{label}: パスが不正です"))?;
    // SAFETY: `c` is a valid NUL-terminated string.
    if unsafe { libc::access(c.as_ptr(), libc::W_OK) } != 0 {
        return Err(format!("{label}: 作成できません ({} に書き込み権限がありません)", anc.display()));
    }
    Ok(())
}

fn check_url(label: &str, raw: &str) -> Result<(), String> {
    let u = reqwest::Url::parse(raw).map_err(|_| format!("{label}: URL として解釈できません"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return Err(format!("{label}: http(s):// で始まる URL を指定してください"));
    }
    Ok(())
}

fn check_host(label: &str, raw: &str) -> Result<(), String> {
    if raw.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    let b = raw.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 253
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'.' || *c == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!("{label}: ホスト名または IP アドレスとして不正です"))
    }
}

fn check_range(label: &str, v: i64, lo: i64, hi: i64) -> Result<(), String> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(format!("{label}: {lo}〜{hi} の範囲で指定してください (指定値 {v})"))
    }
}

fn valid_hhmm(s: &str) -> bool {
    chrono::NaiveTime::parse_from_str(s, "%H:%M").is_ok() && s.len() == 5
}

pub fn validate_settings(s: &Settings) -> Result<(), String> {
    check_path("models_dir", &s.models_dir)?;
    check_path("llama_cpp_dir", &s.llama_cpp_dir)?;
    check_path("vllm_venv", &s.vllm_venv)?;
    check_path("strata_dir", &s.strata_dir)?;
    check_url("hf_endpoint", &s.hf_endpoint)?;
    check_url("ollama_url", &s.ollama_url)?;
    check_host("default_host", &s.default_host)?;
    if s.hf_token.len() > 512 || s.hf_token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("hf_token: 空白・制御文字を含まない 512 文字以内で指定してください".into());
    }
    check_range("port_range_start", s.port_range_start as i64, 1024, 65535)?;
    check_range("max_concurrent_downloads", s.max_concurrent_downloads as i64, 1, 16)?;
    check_range("connections_per_file", s.connections_per_file as i64, 1, 64)?;
    let sh = &s.mode.shared;
    if !(0.1..=1.0).contains(&sh.cpu_fraction) {
        return Err("mode.shared.cpu_fraction: 0.1〜1.0 で指定してください".into());
    }
    check_range("mode.shared.vram_reserve_mb", sh.vram_reserve_mb as i64, 0, 1_048_576)?;
    check_range("mode.shared.gpu_power_percent", sh.gpu_power_percent as i64, 10, 100)?;
    check_range("mode.shared.ubatch_cap", sh.ubatch_cap as i64, 1, 8192)?;
    check_range("mode.shared.nice", sh.nice as i64, 0, 19)?;
    check_range("mode.shared.max_concurrent_downloads", sh.max_concurrent_downloads as i64, 1, 16)?;
    check_range("mode.shared.connections_per_file", sh.connections_per_file as i64, 1, 64)?;
    if let Some(m) = sh.download_mib_per_sec {
        check_range("mode.shared.download_mib_per_sec", m as i64, 1, 100_000)?;
    }
    if !valid_hhmm(&s.mode.schedule.full_start) || !valid_hhmm(&s.mode.schedule.full_end) {
        return Err("mode.schedule: 時刻は HH:MM 形式で指定してください".into());
    }
    if s.extra_env.len() > MAX_ENV_ENTRIES {
        return Err("extra_env が多すぎます".into());
    }
    for k in s.extra_env.keys() {
        validate_env_name(k)?;
    }
    Ok(())
}

/// Apply a partial patch to `current`, returning the validated next settings.
///
/// * unknown top-level keys are rejected,
/// * a masked `hf_token` (`****...`) leaves the stored secret alone, `""` clears it,
/// * `extra_env` (if present) replaces the whole map (masked secret values keep their stored value),
/// * `mode.current` cannot be changed here (use `PUT /api/mode`, which also applies the profile).
pub fn apply_patch(current: &Settings, patch: &Value) -> Result<Settings, ApiError> {
    let Value::Object(pobj) = patch else {
        return Err(ApiError::bad_request("JSON オブジェクトを指定してください"));
    };
    let mut base = settings_value(current);
    let mut pobj = pobj.clone();
    for k in pobj.keys() {
        if base.get(k).is_none() {
            return Err(ApiError::bad_request(format!("未知の設定キー: {k}")));
        }
    }
    if let Some(Value::String(t)) = pobj.get("hf_token") {
        if is_masked(t) {
            pobj.remove("hf_token");
        }
    }
    let new_env = match pobj.remove("extra_env") {
        Some(Value::Object(m)) => Some(build_env(&m, &current.extra_env)?),
        Some(_) => return Err(ApiError::bad_request("extra_env はオブジェクトで指定してください")),
        None => None,
    };
    if let Some(Value::Object(m)) = pobj.get_mut("mode") {
        if let Some(cur) = m.remove("current") {
            if cur != base["mode"]["current"] {
                return Err(ApiError::bad_request("mode.current はここでは変更できません (PUT /api/mode を使用)"));
            }
        }
    }
    crate::config::merge_json(&mut base, &Value::Object(pobj));
    let mut next: Settings = serde_json::from_value(base).map_err(|e| ApiError::bad_request(format!("設定が不正です: {e}")))?;
    if let Some(env) = new_env {
        next.extra_env = env;
    }
    validate_settings(&next).map_err(ApiError::bad_request)?;
    Ok(next)
}

/// Dotted keys whose value differs (names only, never values).
fn changed_keys(old: &Value, new: &Value) -> Vec<String> {
    fn walk(prefix: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) if prefix != "extra_env" => {
                for (k, bv) in y {
                    let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(&key, x.get(k).unwrap_or(&Value::Null), bv, out);
                }
            }
            _ => {
                if a != b {
                    out.push(prefix.to_string());
                }
            }
        }
    }
    let mut out = vec![];
    walk("", old, new, &mut out);
    out
}

// ═════════════════════════════════════════════════════════════════════ schema
struct KeyMeta {
    key: &'static str,
    ty: &'static str,
    desc: &'static str,
    restart: bool,
}

const SCHEMA: &[KeyMeta] = &[
    KeyMeta { key: "models_dir", ty: "path", desc: "モデル (GGUF / safetensors) の保存先ディレクトリ", restart: false },
    KeyMeta { key: "hf_token", ty: "secret", desc: "Hugging Face のアクセストークン (非公開・ゲート付きモデル用)。表示はマスクされます", restart: false },
    KeyMeta { key: "hf_endpoint", ty: "url", desc: "Hugging Face のエンドポイント URL (ミラー利用時に変更)", restart: false },
    KeyMeta { key: "llama_cpp_dir", ty: "path", desc: "llama.cpp のビルド先ディレクトリ", restart: false },
    KeyMeta { key: "vllm_venv", ty: "path", desc: "vLLM 専用 Python 仮想環境のディレクトリ", restart: false },
    KeyMeta { key: "strata_dir", ty: "path", desc: "Strata のチェックアウト先ディレクトリ", restart: false },
    KeyMeta { key: "strata_lazy_load", ty: "bool", desc: "Strata を --lazy で起動 (起動直後は VRAM を使わず、初回リクエストで読み込み)。新規起動から有効", restart: false },
    KeyMeta { key: "strata_idle_unload_s", ty: "integer(0-86400)", desc: "フルモードで Strata が無操作のまま指定秒数を超えたらモデルをアンロード。0 で無効。処理中は解除されません", restart: false },
    KeyMeta { key: "strata_shared_idle_unload_s", ty: "integer(0-86400)", desc: "共有モード時の無操作アンロード秒数 (短め推奨)。0 で無効。モード切替直後から適用されます", restart: false },
    KeyMeta { key: "strata_watchdog", ty: "bool", desc: "ハーネスが起動した Strata が予期せず終了したら自動再起動 (指数バックオフ、10 分で 5 回失敗すると停止)。手動停止では再起動しません", restart: false },
    KeyMeta { key: "strata_swap_on_demand", ty: "bool", desc: "起動していない Strata の設定名がリクエストで指定されたら、起動中の Strata を停止して切り替え (処理中なら最大 60 秒待機)", restart: false },
    KeyMeta { key: "ollama_url", ty: "url", desc: "Ollama API の URL", restart: false },
    KeyMeta { key: "max_concurrent_downloads", ty: "integer(1-16)", desc: "フルモードでの同時ダウンロード数 (反映にはモード再適用か再起動が必要)", restart: true },
    KeyMeta { key: "connections_per_file", ty: "integer(1-64)", desc: "フルモードでの 1 ファイルあたりの並列接続数 (反映にはモード再適用か再起動が必要)", restart: true },
    KeyMeta { key: "default_host", ty: "host", desc: "起動するモデルサーバのバインドアドレス (新規起動から有効)", restart: false },
    KeyMeta { key: "port_range_start", ty: "port(1024-65535)", desc: "モデルサーバに割り当てるポート範囲の開始番号", restart: false },
    KeyMeta { key: "privacy_mode", ty: "bool", desc: "プロンプト/出力をログに残さない (推奨)", restart: false },
    KeyMeta { key: "extra_env", ty: "env-map", desc: "モデルサーバ起動時に追加する環境変数 (新規起動から有効)。/api/admin/env でも編集できます", restart: false },
    KeyMeta { key: "mode", ty: "object", desc: "性能モード設定 (共有モードの上限・スケジュール)。現在のモード切替は /api/mode を使用", restart: true },
];

fn schema_value() -> Value {
    Value::Object(SCHEMA.iter().map(|m| (m.key.to_string(), json!({"type": m.ty, "description": m.desc, "restart_required": m.restart}))).collect())
}

fn restart_required(changed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = changed
        .iter()
        .map(|k| k.split('.').next().unwrap_or(k).to_string())
        .filter(|top| SCHEMA.iter().any(|m| m.key == top && m.restart))
        .collect();
    out.sort();
    out.dedup();
    out
}

// ═════════════════════════════════════════════════════════════════════ config history
const HISTORY_KEEP: usize = 20;

fn history_dir() -> PathBuf {
    HARNESS_HOME.join("config-history")
}

/// Versions are `YYYYMMDDTHHMMSSnnnnnnnnnZ`: digits plus `T`/`Z` only, so never a path.
pub fn valid_version(v: &str) -> bool {
    v.len() == 25 && v.ends_with('Z') && v.as_bytes()[8] == b'T' && v.bytes().enumerate().all(|(i, c)| c.is_ascii_digit() || (i == 8 && c == b'T') || (i == 24 && c == b'Z'))
}

fn history_snapshot(dir: &Path, current: &Settings) -> anyhow::Result<String> {
    std::fs::create_dir_all(dir)?;
    let version = Utc::now().format("%Y%m%dT%H%M%S%9fZ").to_string();
    let path = dir.join(format!("{version}.json"));
    crate::config::write_json_file(&path, current)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let mut all = history_versions(dir);
    all.sort();
    while all.len() > HISTORY_KEEP {
        let old = all.remove(0);
        let _ = std::fs::remove_file(dir.join(format!("{old}.json")));
    }
    Ok(version)
}

fn history_versions(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter_map(|n| n.strip_suffix(".json").map(str::to_string)).filter(|v| valid_version(v)).collect())
        .unwrap_or_default()
}

fn history_list(dir: &Path) -> Vec<Value> {
    let mut v = history_versions(dir);
    v.sort_by(|a, b| b.cmp(a));
    v.into_iter()
        .map(|ver| {
            let md = std::fs::metadata(dir.join(format!("{ver}.json"))).ok();
            let created = md.as_ref().and_then(|m| m.modified().ok()).map(|t| DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Secs, true));
            json!({"version": ver, "created_at": created, "size": md.map(|m| m.len())})
        })
        .collect()
}

fn history_load(dir: &Path, version: &str) -> Result<Settings, ApiError> {
    if !valid_version(version) {
        return Err(ApiError::bad_request("不正な version です"));
    }
    let path = dir.join(format!("{version}.json"));
    if !path.is_file() {
        return Err(ApiError::not_found("その version はありません"));
    }
    crate::config::read_json_file::<Settings>(&path).ok_or_else(|| ApiError::internal("履歴ファイルを読めません"))
}

static CONFIG_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Snapshot the current settings, then persist `next`. Returns the snapshot version.
fn commit(st: &AppState, next: Settings) -> Result<String, ApiError> {
    let prev = st.settings.get();
    let version = history_snapshot(&history_dir(), &prev)?;
    let models = next.models_dir();
    st.settings.modify(|s| *s = next)?;
    let _ = std::fs::create_dir_all(models);
    Ok(version)
}

// ═════════════════════════════════════════════════════════════════════ config handlers
async fn get_config(_a: Admin, State(st): State<AppState>) -> Json<Value> {
    Json(json!({"settings": masked_settings(&st.settings.get()), "schema": schema_value(), "history": history_list(&history_dir())}))
}

async fn put_config(a: Admin, State(st): State<AppState>, Json(patch): Json<Value>) -> ApiResult<Value> {
    let _g = CONFIG_LOCK.lock();
    let cur = st.settings.get();
    let next = apply_patch(&cur, &patch)?;
    let changed = changed_keys(&settings_value(&cur), &settings_value(&next));
    if changed.is_empty() {
        return Ok(Json(json!({"ok": true, "changed": [], "restart_required": [], "version": null, "settings": masked_settings(&cur)})));
    }
    let masked_next = masked_settings(&next);
    let version = commit(&st, next)?;
    audit(&st, &a, "config.update", &format!("changed: {}", changed.join(", ")));
    Ok(Json(json!({"ok": true, "changed": changed, "restart_required": restart_required(&changed), "version": version, "settings": masked_next})))
}

#[derive(Deserialize)]
struct RollbackBody {
    version: String,
}

async fn rollback_config(a: Admin, State(st): State<AppState>, Json(body): Json<RollbackBody>) -> ApiResult<Value> {
    let _g = CONFIG_LOCK.lock();
    let target = history_load(&history_dir(), &body.version)?;
    validate_settings(&target).map_err(|e| ApiError::bad_request(format!("履歴の設定が現在の検証に通りません: {e}")))?;
    let cur = st.settings.get();
    let changed = changed_keys(&settings_value(&cur), &settings_value(&target));
    let masked = masked_settings(&target);
    let snap = commit(&st, target)?;
    audit(&st, &a, "config.rollback", &format!("rollback to {}; changed: {}", body.version, changed.join(", ")));
    Ok(Json(json!({"ok": true, "changed": changed, "restart_required": restart_required(&changed), "version": snap, "settings": masked})))
}

async fn config_history(_a: Admin) -> Json<Value> {
    Json(json!({"history": history_list(&history_dir())}))
}

/// Export without secrets (`hf_token` and secret-looking env values are left out).
fn export_value(s: &Settings) -> Value {
    let mut v = settings_value(s);
    if let Value::Object(m) = &mut v {
        m.remove("hf_token");
    }
    let env: Map<String, Value> = s.extra_env.iter().filter(|(k, _)| !is_secret_name(k)).map(|(k, v)| (k.clone(), json!(v))).collect();
    v["extra_env"] = Value::Object(env);
    json!({"harness_export": 1, "exported_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true), "settings": v})
}

async fn export_config(_a: Admin, State(st): State<AppState>) -> Response {
    let body = serde_json::to_string_pretty(&export_value(&st.settings.get())).unwrap_or_default();
    ([(header::CONTENT_TYPE, "application/json; charset=utf-8"), (header::CONTENT_DISPOSITION, "attachment; filename=\"llm-harness-config.json\"")], body).into_response()
}

/// Turn an exported document into a patch: secrets are never imported, existing secret env
/// entries survive, the mode selection is untouched.
fn import_patch(doc: &Value, current: &Settings) -> Result<Value, ApiError> {
    let inner = doc.get("settings").unwrap_or(doc);
    let Value::Object(m) = inner else {
        return Err(ApiError::bad_request("エクスポートした JSON を指定してください"));
    };
    let mut m = m.clone();
    m.remove("hf_token");
    m.remove("harness_export");
    if let Some(Value::Object(mode)) = m.get_mut("mode") {
        mode.remove("current");
    }
    if let Some(Value::Object(env)) = m.get_mut("extra_env") {
        for (k, v) in &current.extra_env {
            if is_secret_name(k) && !env.contains_key(k) {
                // masked placeholder: build_env resolves it to the stored value
                env.insert(k.clone(), json!(mask_secret(v)));
            }
        }
    }
    Ok(Value::Object(m))
}

async fn import_config(a: Admin, State(st): State<AppState>, Json(doc): Json<Value>) -> ApiResult<Value> {
    let _g = CONFIG_LOCK.lock();
    let cur = st.settings.get();
    let patch = import_patch(&doc, &cur)?;
    let next = apply_patch(&cur, &patch)?;
    let changed = changed_keys(&settings_value(&cur), &settings_value(&next));
    let masked = masked_settings(&next);
    let version = if changed.is_empty() { None } else { Some(commit(&st, next)?) };
    audit(&st, &a, "config.import", &format!("changed: {}", changed.join(", ")));
    Ok(Json(json!({"ok": true, "changed": changed, "restart_required": restart_required(&changed), "version": version, "settings": masked})))
}

// ═════════════════════════════════════════════════════════════════════ env handlers
async fn get_env(_a: Admin, State(st): State<AppState>) -> Json<Value> {
    Json(json!({"env": masked_env(&st.settings.get().extra_env), "deny": ENV_DENY, "name_pattern": "[A-Z_][A-Z0-9_]*"}))
}

#[derive(Deserialize)]
struct EnvBody {
    env: Map<String, Value>,
}

async fn put_env(a: Admin, State(st): State<AppState>, Json(body): Json<EnvBody>) -> ApiResult<Value> {
    let _g = CONFIG_LOCK.lock();
    let cur = st.settings.get();
    let env = build_env(&body.env, &cur.extra_env)?;
    let mut changed: Vec<String> = env.iter().filter(|(k, v)| cur.extra_env.get(*k) != Some(*v)).map(|(k, _)| k.clone()).collect();
    changed.extend(cur.extra_env.keys().filter(|k| !env.contains_key(*k)).map(|k| format!("-{k}")));
    if !changed.is_empty() {
        let mut next = cur.clone();
        next.extra_env = env;
        commit(&st, next)?;
        audit(&st, &a, "env.update", &format!("changed: {}", changed.join(", ")));
    }
    Ok(Json(json!({"ok": true, "changed": changed, "env": masked_env(&st.settings.get().extra_env)})))
}

// ═════════════════════════════════════════════════════════════════════ system info
static START: LazyLock<(Instant, SystemTime)> = LazyLock::new(|| (Instant::now(), SystemTime::now()));

/// Sum of file sizes below `root`, bounded by entry count and time. Symlinks are not followed.
fn dir_usage(root: &Path, max_entries: usize, budget: Duration) -> (u64, bool) {
    let t0 = Instant::now();
    let mut total = 0u64;
    let mut seen = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > max_entries || t0.elapsed() > budget {
                return (total, true);
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (total, false)
}

fn disk_space(path: &Path) -> Option<(u64, u64)> {
    let anc = path.ancestors().find(|a| a.exists())?;
    let c = std::ffi::CString::new(anc.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid C string and out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize;
    Some((st.f_blocks * frsize, st.f_bavail * frsize))
}

fn data_dirs(s: &Settings) -> Vec<Value> {
    let dirs: Vec<(&str, PathBuf)> = vec![
        ("harness_home", HARNESS_HOME.clone()),
        ("models_dir", s.models_dir()),
        ("llama_cpp_dir", s.llama_cpp_dir()),
        ("vllm_venv", s.vllm_venv()),
        ("strata_dir", s.strata_dir()),
    ];
    dirs.into_iter()
        .map(|(key, path)| {
            let exists = path.is_dir();
            let (used, truncated) = if exists { dir_usage(&path, 300_000, Duration::from_secs(3)) } else { (0, false) };
            let disk = disk_space(&path);
            json!({
                "key": key,
                "path": path.display().to_string(),
                "exists": exists,
                "used_bytes": used,
                "truncated": truncated,
                "disk_total_bytes": disk.map(|d| d.0),
                "disk_free_bytes": disk.map(|d| d.1),
            })
        })
        .collect()
}

/// Mask the value following sensitive flags (`--token X`, `--api-key=X`, `TOKEN=X`).
pub fn redact_cmdline(args: &[String]) -> Vec<String> {
    let sensitive = |name: &str| {
        let n = name.trim_start_matches('-').to_ascii_lowercase();
        ["token", "key", "secret", "password", "passwd", "auth", "credential"].iter().any(|k| n.contains(k))
    };
    let mut out = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for a in args {
        if mask_next {
            out.push("****".to_string());
            mask_next = false;
        } else if let Some((l, _)) = a.split_once('=') {
            if sensitive(l) {
                out.push(format!("{l}=****"));
            } else {
                out.push(a.clone());
            }
        } else if a.starts_with('-') && sensitive(a) {
            out.push(a.clone());
            mask_next = true;
        } else {
            out.push(a.clone());
        }
    }
    out
}

fn child_processes() -> Vec<Value> {
    let me = std::process::id() as i32;
    let mut parent: HashMap<i32, i32> = HashMap::new();
    let mut comm: HashMap<i32, String> = HashMap::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return vec![] };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let (Some(l), Some(r)) = (stat.find('('), stat.rfind(')')) else { continue };
        let rest: Vec<&str> = stat[r + 1..].split_whitespace().collect();
        if let Some(ppid) = rest.get(1).and_then(|p| p.parse::<i32>().ok()) {
            parent.insert(pid, ppid);
            comm.insert(pid, stat[l + 1..r].to_string());
        }
    }
    let mut found: Vec<i32> = vec![];
    let mut frontier = vec![me];
    while let Some(p) = frontier.pop() {
        for (&c, &pp) in &parent {
            if pp == p && !found.contains(&c) {
                found.push(c);
                frontier.push(c);
            }
        }
    }
    found.sort();
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    found
        .into_iter()
        .map(|pid| {
            let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let args: Vec<String> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
            let mut cmd = redact_cmdline(&args).join(" ");
            if cmd.is_empty() {
                cmd = format!("[{}]", comm.get(&pid).cloned().unwrap_or_default());
            }
            let cmd: String = cmd.chars().take(300).collect();
            let rss = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok().and_then(|s| s.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok())).map(|p| p * page);
            json!({"pid": pid, "ppid": parent.get(&pid), "cmd": cmd, "rss_bytes": rss})
        })
        .collect()
}

pub fn parse_systemctl_show(text: &str) -> HashMap<String, String> {
    text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}

const UNIT: &str = "llm-harness";

async fn systemctl_show(user: bool) -> Option<HashMap<String, String>> {
    let mut cmd = tokio::process::Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    cmd.args(["show", UNIT, "--no-pager", "-p", "LoadState", "-p", "ActiveState", "-p", "SubState", "-p", "MainPID", "-p", "NRestarts", "-p", "UnitFileState", "-p", "Restart"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(3), cmd.output()).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    let m = parse_systemctl_show(&String::from_utf8_lossy(&out.stdout));
    (m.get("LoadState").is_some_and(|s| s != "not-found")).then_some(m)
}

/// `(scope, properties)` of the `llm-harness` unit, user scope first. `None` when systemd or the unit is absent.
async fn systemd_unit() -> Option<(&'static str, HashMap<String, String>)> {
    if let Some(m) = systemctl_show(true).await {
        return Some(("user", m));
    }
    systemctl_show(false).await.map(|m| ("system", m))
}

fn under_systemd() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
}

async fn get_system(_a: Admin, State(st): State<AppState>) -> ApiResult<Value> {
    let settings = st.settings.get();
    let (dirs, procs) = tokio::task::spawn_blocking(move || (data_dirs(&settings), child_processes())).await.map_err(|e| ApiError::internal(e.to_string()))?;
    let unit = systemd_unit().await;
    let systemd = match unit {
        Some((scope, m)) => json!({
            "available": true,
            "scope": scope,
            "unit": UNIT,
            "active_state": m.get("ActiveState"),
            "sub_state": m.get("SubState"),
            "main_pid": m.get("MainPID").and_then(|p| p.parse::<u32>().ok()),
            "restarts": m.get("NRestarts").and_then(|p| p.parse::<u32>().ok()),
            "unit_file_state": m.get("UnitFileState"),
            "restart_policy": m.get("Restart"),
        }),
        None => json!({"available": false}),
    };
    let tun = st.tunnel.status_json().await;
    let prof = st.profile();
    let (started_inst, started_sys) = *START;
    Ok(Json(json!({
        "hostname": sysinfo::System::host_name(),
        "kernel": sysinfo::System::kernel_version(),
        "os": sysinfo::System::long_os_version(),
        "uptime_secs": sysinfo::System::uptime(),
        "harness": {
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "started_at": DateTime::<Utc>::from(started_sys).to_rfc3339_opts(SecondsFormat::Secs, true),
            "uptime_secs": started_inst.elapsed().as_secs(),
            "binary": std::env::current_exe().ok().map(|p| p.display().to_string()),
            "repo_root": REPO_ROOT.display().to_string(),
            "home": HARNESS_HOME.display().to_string(),
            "under_systemd": under_systemd(),
        },
        "dirs": dirs,
        "processes": procs,
        "systemd": systemd,
        "tunnel": {
            "state": tun.get("state"),
            "provider": tun.get("provider"),
            "public_url": tun.get("public_url"),
            "connected_since": tun.get("connected_since"),
            "last_error": tun.get("last_error"),
        },
        "mode": {"current": prof.mode, "profile": prof},
    })))
}

// ═════════════════════════════════════════════════════════════════════ audit endpoint
#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<usize>,
}

async fn get_audit(_a: Admin, Query(q): Query<AuditQuery>) -> ApiResult<Value> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let entries = tokio::task::spawn_blocking(move || AUDIT.read(limit)).await.map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({"entries": entries})))
}

// ═════════════════════════════════════════════════════════════════════ logs
/// Allowlisted log sources. There is deliberately no way to name a path.
#[derive(Debug, PartialEq)]
pub enum Source {
    Harness,
    Tunnel,
    Job(String),
}

impl Source {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "harness" => Some(Self::Harness),
            "tunnel" => Some(Self::Tunnel),
            _ => {
                let id = s.strip_prefix("job:")?;
                (!id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')).then(|| Self::Job(id.to_string()))
            }
        }
    }
}

#[derive(Deserialize)]
struct LogsQuery {
    source: Option<String>,
    lines: Option<usize>,
}

async fn get_logs(_a: Admin, State(st): State<AppState>, Query(q): Query<LogsQuery>) -> ApiResult<Value> {
    let name = q.source.unwrap_or_else(|| "harness".into());
    let n = q.lines.unwrap_or(200).clamp(1, 2000);
    let Some(src) = Source::parse(&name) else {
        return Err(ApiError::bad_request("source は harness / tunnel / job:<id> のいずれかです"));
    };
    let lines: Vec<String> = match src {
        Source::Harness => ring_tail(n),
        Source::Tunnel => {
            let v = st.tunnel.logs_since(0);
            let all: Vec<String> = v["lines"].as_array().map(|a| a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect()).unwrap_or_default();
            all[all.len().saturating_sub(n)..].to_vec()
        }
        Source::Job(id) => {
            let run = st.instances.job_run(&id).ok_or_else(|| ApiError::not_found("job が見つかりません"))?;
            let v = serde_json::to_value(run.tail(0, n)).unwrap_or(Value::Null);
            v["lines"].as_array().map(|a| a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect()).unwrap_or_default()
        }
    };
    Ok(Json(json!({"source": name, "lines": lines})))
}

async fn log_sources(_a: Admin, State(st): State<AppState>) -> Json<Value> {
    let mut sources = vec![json!({"id": "harness", "title": "ハーネス本体"}), json!({"id": "tunnel", "title": "トンネル"})];
    for r in st.instances.job_runs() {
        if let Some(id) = r["id"].as_str() {
            sources.push(json!({"id": format!("job:{id}"), "title": r["name"], "status": r["status"]}));
        }
    }
    Json(json!({"sources": sources}))
}

// ═════════════════════════════════════════════════════════════════════ actions
/// Strict allowlist of maintenance actions: `(name, description)`.
pub const ACTIONS: &[(&str, &str)] = &[
    ("restart-harness", "ハーネスを再起動 (systemd 配下でのみ)"),
    ("stop-all-servers", "稼働中のモデルサーバとセットアップジョブをすべて停止"),
    ("clear-logs", "メモリ上のハーネスログを消去"),
    ("drop-caches-hint", "ページキャッシュ解放の手順を表示 (何も実行しません)"),
    ("update-harness", "git pull --ff-only → ./harness.sh install をジョブとして実行"),
    ("gpu-reset-persistence", "nvidia-smi -pm 1 (GPU persistence mode を有効化)"),
];

pub const RESTART_EXIT_CODE: i32 = 75;
static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Checked by `main` after graceful shutdown: exit non-zero so systemd revives the unit.
pub fn restart_requested() -> bool {
    RESTART_REQUESTED.load(Ordering::SeqCst)
}

pub fn restart_policy_ok(policy: &str) -> bool {
    matches!(policy, "always" | "on-failure")
}

struct Outcome {
    ok: bool,
    message: String,
    extra: Option<(&'static str, Value)>,
}

fn done(message: impl Into<String>) -> Outcome {
    Outcome { ok: true, message: message.into(), extra: None }
}
fn refused(message: impl Into<String>) -> Outcome {
    Outcome { ok: false, message: message.into(), extra: None }
}

async fn list_actions(_a: Admin) -> Json<Value> {
    Json(json!({"actions": ACTIONS.iter().map(|(id, d)| json!({"id": id, "description": d})).collect::<Vec<_>>()}))
}

async fn run_action(a: Admin, State(st): State<AppState>, axum::extract::Path(action): axum::extract::Path<String>) -> Response {
    let Some((name, _)) = ACTIONS.iter().find(|(n, _)| *n == action) else {
        return ApiError::not_found("unknown action").into_response();
    };
    let out = match *name {
        "restart-harness" => action_restart().await,
        "stop-all-servers" => {
            st.instances.stop_all().await;
            done("すべてのモデルサーバとジョブを停止しました")
        }
        "clear-logs" => {
            RING.lock().clear();
            done("メモリ上のハーネスログを消去しました (トンネル/ジョブのログは各セッションで保持されます)")
        }
        "drop-caches-hint" => done("何も実行していません。WSL2 のメモリ回収: Windows 側で `wsl --shutdown`、または .wslconfig の autoMemoryReclaim=gradual を設定。ページキャッシュ解放は WSL 内で `sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'`。"),
        "update-harness" => action_update(&st).await,
        "gpu-reset-persistence" => action_gpu().await,
        _ => unreachable!("allowlist checked above"),
    };
    audit(&st, &a, &format!("action.{name}"), &format!("{} ({})", if out.ok { "ok" } else { "refused/failed" }, out.message.chars().take(120).collect::<String>()));
    let mut body = json!({"ok": out.ok, "message": out.message});
    if !out.ok {
        body["detail"] = body["message"].clone();
    }
    if let Some((k, v)) = out.extra {
        body[k] = v;
    }
    (if out.ok { StatusCode::OK } else { StatusCode::CONFLICT }, Json(body)).into_response()
}

async fn action_restart() -> Outcome {
    if !under_systemd() {
        return refused("systemd 配下で動いていないため再起動できません (戻ってこられなくなります)。`./harness.sh service` で登録するか、ワークステーション上で手動再起動してください");
    }
    let Some((_, m)) = systemd_unit().await else {
        return refused("systemd のユニット llm-harness の状態を確認できないため再起動を拒否しました");
    };
    if m.get("MainPID").and_then(|p| p.parse::<u32>().ok()) != Some(std::process::id()) {
        return refused("このプロセスは systemd ユニット llm-harness の本体ではないため再起動できません");
    }
    let policy = m.get("Restart").map(String::as_str).unwrap_or("no");
    if !restart_policy_ok(policy) {
        return refused(format!("ユニットの Restart={policy} では自動復帰しません。Restart=always (または on-failure) に設定してください"));
    }
    RESTART_REQUESTED.store(true, Ordering::SeqCst);
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        tracing::warn!("admin: restart requested, shutting down gracefully (exit {RESTART_EXIT_CODE})");
        // Hard deadline in case open connections keep graceful shutdown from finishing.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(20));
            std::process::exit(RESTART_EXIT_CODE);
        });
        // SAFETY: signalling our own process; main's handler performs the graceful shutdown.
        unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
    });
    done("再起動します。数秒後にページを再読み込みしてください")
}

async fn action_update(st: &AppState) -> Outcome {
    if !REPO_ROOT.join(".git").exists() {
        return refused(format!("{} は git リポジトリではありません", REPO_ROOT.display()));
    }
    match st.instances.run_job(st, "update-harness").await {
        Ok(run) => Outcome {
            ok: true,
            message: "更新ジョブを開始しました。完了後に restart-harness を実行してください (ログ: source=job:<id>)".into(),
            extra: Some(("job_id", json!(run.id))),
        },
        Err(e) => refused(format!("{e:#}")),
    }
}

async fn action_gpu() -> Outcome {
    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args(["-pm", "1"]).stdin(std::process::Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(15), cmd.output()).await {
        Err(_) => refused("nvidia-smi がタイムアウトしました"),
        Ok(Err(e)) => refused(format!("nvidia-smi を実行できません: {e}")),
        Ok(Ok(o)) => {
            let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            let text: String = text.trim().chars().take(400).collect();
            if o.status.success() {
                done(if text.is_empty() { "persistence mode を有効化しました".to_string() } else { text })
            } else {
                refused(format!("失敗しました (権限不足の可能性): {text}"))
            }
        }
    }
}

// ═════════════════════════════════════════════════════════════════════ router
pub fn router() -> Router<AppState> {
    LazyLock::force(&START);
    Router::new()
        .route("/api/admin/config", get(get_config).put(put_config))
        .route("/api/admin/config/rollback", post(rollback_config))
        .route("/api/admin/config/history", get(config_history))
        .route("/api/admin/config/export", get(export_config))
        .route("/api/admin/config/import", post(import_config))
        .route("/api/admin/env", get(get_env).put(put_env))
        .route("/api/admin/system", get(get_system))
        .route("/api/admin/audit", get(get_audit))
        .route("/api/admin/logs", get(get_logs))
        .route("/api/admin/logs/sources", get(log_sources))
        .route("/api/admin/actions", get(list_actions))
        .route("/api/admin/actions/{action}", post(run_action))
        .route_layer(middleware::from_fn(session_only))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring buffer is process-global; tests that fill or read it must not interleave.
    static RING_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn router_builds_without_route_conflicts() {
        let _ = router();
    }
    use axum::middleware::from_fn;

    const TOKEN: &str = "hf_SuperSecretTokenValue1234";

    fn settings_in(dir: &Path) -> Settings {
        Settings {
            models_dir: dir.join("models").display().to_string(),
            llama_cpp_dir: dir.join("llama").display().to_string(),
            vllm_venv: dir.join("venv").display().to_string(),
            strata_dir: dir.join("strata").display().to_string(),
            hf_token: TOKEN.into(),
            ..Default::default()
        }
    }

    #[test]
    fn masking_shape() {
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("short"), "****");
        assert_eq!(mask_secret(TOKEN), "****1234");
        let d = tempfile::tempdir().unwrap();
        let mut s = settings_in(d.path());
        s.extra_env.insert("OPENAI_API_KEY".into(), "sk-abcdefghijklmnop".into());
        s.extra_env.insert("CUDA_VISIBLE_DEVICES".into(), "0".into());
        let v = masked_settings(&s);
        assert_eq!(v["hf_token"], "****1234");
        assert_eq!(v["extra_env"]["OPENAI_API_KEY"], "****mnop");
        assert_eq!(v["extra_env"]["CUDA_VISIBLE_DEVICES"], "0");
        assert!(!v.to_string().contains("SuperSecret"));
    }

    #[test]
    fn masked_roundtrip_does_not_clobber_secrets() {
        let d = tempfile::tempdir().unwrap();
        let mut s = settings_in(d.path());
        s.extra_env.insert("MY_SECRET".into(), "topsecretvalue99".into());
        let shown = masked_settings(&s);
        // The UI sends the whole masked document back with one real change.
        let mut patch = shown.clone();
        patch["port_range_start"] = json!(9000);
        let next = apply_patch(&s, &patch).unwrap();
        assert_eq!(next.hf_token, TOKEN);
        assert_eq!(next.extra_env["MY_SECRET"], "topsecretvalue99");
        assert_eq!(next.port_range_start, 9000);
        // explicit new value and explicit clear
        assert_eq!(apply_patch(&s, &json!({"hf_token": "hf_newtoken"})).unwrap().hf_token, "hf_newtoken");
        assert_eq!(apply_patch(&s, &json!({"hf_token": ""})).unwrap().hf_token, "");
        // masked value for a secret env key that does not exist is rejected
        assert!(apply_patch(&s, &json!({"extra_env": {"NEW_TOKEN": "****abcd"}})).is_err());
    }

    #[test]
    fn config_validation() {
        let d = tempfile::tempdir().unwrap();
        let s = settings_in(d.path());
        let bad = |p: Value| assert!(apply_patch(&s, &p).is_err(), "{p}");
        bad(json!({"port_range_start": 80}));
        bad(json!({"port_range_start": 70000}));
        bad(json!({"port_range_start": "x"}));
        bad(json!({"max_concurrent_downloads": 0}));
        bad(json!({"max_concurrent_downloads": 1000}));
        bad(json!({"connections_per_file": 65}));
        bad(json!({"default_host": "not a host!"}));
        bad(json!({"default_host": ""}));
        bad(json!({"hf_endpoint": "ftp://x"}));
        bad(json!({"ollama_url": "nonsense"}));
        bad(json!({"models_dir": "relative/path"}));
        bad(json!({"models_dir": ""}));
        bad(json!({"models_dir": "/a/../b"}));
        bad(json!({"unknown_key": 1}));
        bad(json!({"mode": {"shared": {"cpu_fraction": 5.0}}}));
        bad(json!({"mode": {"shared": {"nice": 99}}}));
        bad(json!({"mode": {"schedule": {"full_start": "25:99"}}}));
        bad(json!({"mode": {"current": "shared"}}));
        bad(json!("string"));
        // good ones
        let f = d.path().join("file");
        std::fs::write(&f, "x").unwrap();
        bad(json!({"models_dir": f.display().to_string()}));
        bad(json!({"models_dir": f.join("sub").display().to_string()}));
        let ok = apply_patch(&s, &json!({"default_host": "0.0.0.0", "port_range_start": 9100, "ollama_url": "http://127.0.0.1:11434", "models_dir": d.path().join("newmodels").display().to_string(), "mode": {"shared": {"nice": 5}, "current": "full"}})).unwrap();
        assert_eq!((ok.default_host.as_str(), ok.port_range_start, ok.mode.shared.nice), ("0.0.0.0", 9100, 5));
        assert!(apply_patch(&s, &json!({"default_host": "::1"})).is_ok());
        assert!(apply_patch(&s, &json!({"default_host": "my-host.local"})).is_ok());
    }

    #[test]
    fn env_validation() {
        for good in ["FOO", "_X", "CUDA_VISIBLE_DEVICES", "PATH", "A1_B2"] {
            assert!(validate_env_name(good).is_ok(), "{good}");
        }
        for bad in ["", "foo", "1A", "A-B", "A B", "LD_PRELOAD", "LD_AUDIT", "A=B", "É"] {
            assert!(validate_env_name(bad).is_err(), "{bad}");
        }
        let mut m = Map::new();
        m.insert("FOO".into(), json!(1));
        assert!(build_env(&m, &BTreeMap::new()).is_err(), "non-string value");
        let mut m = Map::new();
        m.insert("LD_PRELOAD".into(), json!("/x.so"));
        assert!(build_env(&m, &BTreeMap::new()).is_err());
        let existing = BTreeMap::from([("API_KEY".to_string(), "realvalue12345678".to_string())]);
        let mut m = Map::new();
        m.insert("API_KEY".into(), json!("****5678"));
        m.insert("OTHER".into(), json!("v"));
        let out = build_env(&m, &existing).unwrap();
        assert_eq!(out["API_KEY"], "realvalue12345678");
        assert_eq!(out["OTHER"], "v");
        assert!(is_secret_name("hf_token") && is_secret_name("MY_SECRET_X") && is_secret_name("API_KEY") && !is_secret_name("CUDA_VISIBLE_DEVICES"));
    }

    #[test]
    fn history_rollback_and_pruning() {
        let d = tempfile::tempdir().unwrap();
        let hist = d.path().join("config-history");
        let mut s = settings_in(d.path());
        let first = history_snapshot(&hist, &s).unwrap();
        assert!(valid_version(&first), "{first}");
        s.port_range_start = 9999;
        let second = history_snapshot(&hist, &s).unwrap();
        assert!(second > first);
        let back = history_load(&hist, &first).unwrap();
        assert_eq!(back.port_range_start, Settings::default().port_range_start);
        assert_eq!(back.hf_token, TOKEN, "history keeps the real secret so rollback restores it");
        assert_eq!(history_list(&hist).len(), 2);
        assert_eq!(history_list(&hist)[0]["version"], second.as_str());
        for _ in 0..25 {
            history_snapshot(&hist, &s).unwrap();
        }
        assert_eq!(history_versions(&hist).len(), HISTORY_KEEP);
        assert!(history_load(&hist, &first).is_err(), "oldest pruned");
        for bad in ["../x", "20260101T00000000000000000", "", "../../etc/passwd", "2026010100000000000000000Z/", "abcdefghTjklmnopqrstuvwxZ"] {
            assert!(history_load(&hist, bad).is_err(), "{bad}");
            assert!(!valid_version(bad));
        }
    }

    #[test]
    fn export_excludes_secrets_and_import_roundtrips() {
        let d = tempfile::tempdir().unwrap();
        let mut s = settings_in(d.path());
        s.extra_env.insert("MY_API_KEY".into(), "keepme-12345678".into());
        s.extra_env.insert("OMP_NUM_THREADS".into(), "4".into());
        let ex = export_value(&s);
        let text = ex.to_string();
        assert!(!text.contains("SuperSecret") && !text.contains("keepme") && !text.contains("hf_token"));
        assert_eq!(ex["settings"]["extra_env"]["OMP_NUM_THREADS"], "4");
        // import into a different config: secrets of the target survive
        let mut target = settings_in(d.path());
        target.port_range_start = 9500;
        target.hf_token = "hf_target_secret_value".into();
        target.extra_env.insert("MY_API_KEY".into(), "target-key-123456".into());
        let patch = import_patch(&ex, &target).unwrap();
        let next = apply_patch(&target, &patch).unwrap();
        assert_eq!(next.hf_token, "hf_target_secret_value");
        assert_eq!(next.extra_env["MY_API_KEY"], "target-key-123456");
        assert_eq!(next.extra_env["OMP_NUM_THREADS"], "4");
        assert_eq!(next.port_range_start, s.port_range_start);
        // raw (unwrapped) documents work too, bad ones don't
        assert!(import_patch(&json!({"port_range_start": 9001}), &target).is_ok());
        assert!(import_patch(&json!([1]), &target).is_err());
    }

    #[test]
    fn changed_keys_and_restart_flags() {
        let d = tempfile::tempdir().unwrap();
        let s = settings_in(d.path());
        let n = apply_patch(&s, &json!({"port_range_start": 9100, "mode": {"shared": {"nice": 3}}, "hf_token": "hf_other_value_xx"})).unwrap();
        let mut ch = changed_keys(&settings_value(&s), &settings_value(&n));
        ch.sort();
        assert_eq!(ch, vec!["hf_token", "mode.shared.nice", "port_range_start"]);
        assert_eq!(restart_required(&ch), vec!["mode"]);
        assert!(changed_keys(&settings_value(&s), &settings_value(&s)).is_empty());
        assert!(SCHEMA.iter().all(|m| !m.desc.is_ascii()), "descriptions are Japanese");
        let keys: Vec<&str> = SCHEMA.iter().map(|m| m.key).collect();
        for k in settings_value(&s).as_object().unwrap().keys() {
            assert!(keys.contains(&k.as_str()), "schema covers {k}");
        }
        for k in ["strata_lazy_load", "strata_idle_unload_s", "strata_shared_idle_unload_s", "strata_watchdog", "strata_swap_on_demand"] {
            let meta = SCHEMA.iter().find(|m| m.key == k).unwrap_or_else(|| panic!("schema entry for {k}"));
            assert!(!meta.restart, "{k} applies to new launches / live, no restart");
        }
    }

    #[test]
    fn audit_log_jsonl_rotation_and_no_secrets() {
        let d = tempfile::tempdir().unwrap();
        let log = AuditLog::new(d.path().join("audit.log"));
        let secrets = vec![TOKEN.to_string()];
        log.append("1.2.3.4", "admin", "config.update", "changed: hf_token, port_range_start", &secrets);
        // even a careless summary cannot leak a known secret
        log.append("1.2.3.4", "admin", "x", &format!("oops {TOKEN}\nnewline"), &secrets);
        let raw = std::fs::read_to_string(d.path().join("audit.log")).unwrap();
        assert!(!raw.contains(TOKEN));
        assert_eq!(raw.lines().count(), 2, "one record per line");
        let entries = log.read(10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1]["action"], "config.update");
        assert_eq!(entries[1]["ip"], "1.2.3.4");
        assert!(entries[0]["ts"].is_string() && entries[0]["user"] == "admin");
        assert_eq!(log.read(1).len(), 1);
        // rotation
        let big = "x".repeat(1024 * 1024);
        std::fs::write(d.path().join("audit.log"), format!("{big}\n").repeat(6)).unwrap();
        log.append("ip", "u", "after.rotate", "s", &[]);
        assert!(d.path().join("audit.log.1").is_file());
        assert_eq!(std::fs::read_to_string(d.path().join("audit.log")).unwrap().lines().count(), 1);
        assert_eq!(log.read(5)[0]["action"], "after.rotate");
    }

    #[test]
    fn config_update_summary_never_contains_values() {
        let d = tempfile::tempdir().unwrap();
        let s = settings_in(d.path());
        let n = apply_patch(&s, &json!({"hf_token": "hf_brand_new_secret_999", "extra_env": {"MY_TOKEN": "tok-value-12345"}})).unwrap();
        let ch = changed_keys(&settings_value(&s), &settings_value(&n));
        let summary = format!("changed: {}", ch.join(", "));
        assert!(!summary.contains("brand_new") && !summary.contains("tok-value"));
        assert!(summary.contains("hf_token") && summary.contains("extra_env"));
    }

    #[test]
    fn action_allowlist() {
        let names: Vec<&str> = ACTIONS.iter().map(|a| a.0).collect();
        assert_eq!(names, vec!["restart-harness", "stop-all-servers", "clear-logs", "drop-caches-hint", "update-harness", "gpu-reset-persistence"]);
        for bad in ["sh", "exec", "../restart-harness", "restart-harness ", "RESTART-HARNESS", ""] {
            assert!(!ACTIONS.iter().any(|(n, _)| *n == bad), "{bad}");
        }
        assert!(restart_policy_ok("always") && restart_policy_ok("on-failure"));
        assert!(!restart_policy_ok("no") && !restart_policy_ok("on-abnormal"));
        assert!(crate::runtimes::job("update-harness").is_some());
        assert!(!crate::runtimes::job("update-harness").unwrap().sudo);
    }

    #[test]
    fn log_source_allowlist() {
        assert_eq!(Source::parse("harness"), Some(Source::Harness));
        assert_eq!(Source::parse("tunnel"), Some(Source::Tunnel));
        assert_eq!(Source::parse("job:abc123"), Some(Source::Job("abc123".into())));
        for bad in ["", "/etc/passwd", "../../etc/shadow", "file:/etc/passwd", "job:", "job:../x", "job:a/b", "job:a b", "Harness", "tunnel ", "instance:1", &format!("job:{}", "a".repeat(33))] {
            assert_eq!(Source::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn ring_buffer_keeps_last_lines() {
        let _g = RING_TEST_LOCK.lock();
        {
            let mut r = RING.lock();
            r.clear();
            for i in 0..RING_CAP + 50 {
                if r.len() >= RING_CAP {
                    r.pop_front();
                }
                r.push_back(format!("line {i}"));
            }
        }
        let t = ring_tail(3);
        assert_eq!(t, vec![format!("line {}", RING_CAP + 47), format!("line {}", RING_CAP + 48), format!("line {}", RING_CAP + 49)]);
        assert_eq!(ring_tail(10_000).len(), RING_CAP);
        RING.lock().clear();
    }

    #[test]
    fn ring_layer_captures_events() {
        let _g = RING_TEST_LOCK.lock();
        use tracing_subscriber::prelude::*;
        RING.lock().clear();
        let sub = tracing_subscriber::registry().with(RingLayer);
        tracing::subscriber::with_default(sub, || {
            tracing::info!(port = 8787, "hello ring");
        });
        let t = ring_tail(5);
        assert!(t.iter().any(|l| l.contains("hello ring") && l.contains("port=8787") && l.contains("INFO")), "{t:?}");
    }

    #[test]
    fn cmdline_redaction() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(redact_cmdline(&a(&["cloudflared", "tunnel", "run", "--token", "eyJSECRET"])).join(" "), "cloudflared tunnel run --token ****");
        assert_eq!(redact_cmdline(&a(&["ngrok", "--authtoken=abc123", "http", "80"])).join(" "), "ngrok --authtoken=**** http 80");
        assert_eq!(redact_cmdline(&a(&["x", "API_KEY=zzz", "-m", "model.gguf"])).join(" "), "x API_KEY=**** -m model.gguf");
        assert!(!redact_cmdline(&a(&["svc", "--api-key", "SECRETVAL"])).join(" ").contains("SECRETVAL"));
    }

    #[test]
    fn systemctl_parse() {
        let m = parse_systemctl_show("LoadState=loaded\nActiveState=active\nMainPID=42\nRestart=always\n");
        assert_eq!(m["Restart"], "always");
        assert_eq!(m["MainPID"], "42");
    }

    #[test]
    fn dir_usage_counts_and_bounds() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/x"), vec![0u8; 100]).unwrap();
        std::fs::write(d.path().join("a/b/y"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_usage(d.path(), 1000, Duration::from_secs(5)), (150, false));
        assert!(dir_usage(d.path(), 1, Duration::from_secs(5)).1, "entry cap reports truncation");
        assert!(disk_space(d.path()).is_some_and(|(t, f)| t > 0 && f <= t));
    }

    // ── session-only gate (real HTTP server, principal injected by a test layer) ──
    async fn inject(mut req: Request, next: Next) -> Response {
        let kind = req.headers().get("x-test-kind").and_then(|v| v.to_str().ok()).map(str::to_string);
        let p = match kind.as_deref() {
            Some("session") => Some(Principal { kind: "session", name: "admin".into(), session_id: Some("s1".into()) }),
            Some("api_key") => Some(Principal { kind: "api_key", name: "k".into(), session_id: None }),
            Some("local") => Some(Principal { kind: "local", name: "local".into(), session_id: None }),
            _ => None,
        };
        if let Some(p) = p {
            req.extensions_mut().insert(p);
        }
        next.run(req).await
    }

    #[tokio::test]
    async fn admin_routes_reject_api_keys_and_anonymous() {
        let admin_like: Router = Router::new().route("/api/admin/ping", get(|_a: Admin| async { "pong" })).route_layer(middleware::from_fn(session_only)).layer(from_fn(inject));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, admin_like.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap() });
        let http = reqwest::Client::new();
        let call = |kind: Option<&'static str>| {
            let mut rb = http.get(format!("http://{addr}/api/admin/ping"));
            if let Some(k) = kind {
                rb = rb.header("x-test-kind", k);
            }
            async move { rb.send().await.unwrap().status().as_u16() }
        };
        assert_eq!(call(Some("session")).await, 200);
        assert_eq!(call(Some("api_key")).await, 403);
        assert_eq!(call(Some("local")).await, 403);
        assert_eq!(call(None).await, 401);
    }
}
