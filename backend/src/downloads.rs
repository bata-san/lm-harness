//! Download manager.
//!
//! * Hugging Face files are fetched with several parallel HTTP Range connections per file
//!   (the HF/Xet CDN throttles single connections) into a pre-allocated `<file>.part`.
//!   Per-segment progress is persisted, so pause / harness restart / WSL shutdown all resume
//!   where they left off.
//! * Ollama pulls are proxied through the Ollama API so their progress shows up in the same
//!   queue.
//! * Limits come from the live [`ModeProfile`] (`watch` channel): the scheduler honours
//!   `max_concurrent_downloads` immediately (surplus tasks are put back in the queue, keeping
//!   their progress), the shared [`RateLimiter`] honours `download_bytes_per_sec` on every
//!   network chunk, and `connections_per_file` applies to files planned afterwards.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail};
use axum::extract::{Path as AxPath, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use parking_lot::Mutex;
use reqwest::header::{CONTENT_RANGE, RANGE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{watch, Notify};
use tokio::task::{JoinHandle, JoinSet};

use crate::config::{read_json_file, write_json_file, Settings, SettingsStore, HARNESS_HOME};
use crate::error::{ok, ApiError, ApiResult};
use crate::hf::{self, HfFile, HfRepo};
use crate::mode::ModeProfile;
use crate::state::AppState;

/// Smallest range segment worth a connection of its own.
pub const MIN_SEGMENT: u64 = 64 * 1024 * 1024;
/// Network chunks are batched into buffers of about this size before hitting the disk.
const WRITE_BUF: usize = 4 * 1024 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRIES: u32 = 8;
const SPEED_WINDOW: Duration = Duration::from_secs(10);
const HASH_BUF: usize = 16 * 1024 * 1024;
const PERSIST_EVERY: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Verifying,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    /// Occupies a download slot.
    fn running(self) -> bool {
        matches!(self, Status::Downloading | Status::Verifying)
    }
    /// Not finished (it may still hold a `.part` file whose segments must stay consistent).
    fn unfinished(self) -> bool {
        !matches!(self, Status::Completed | Status::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Hf,
    Ollama,
}

/// Inclusive byte range `[start, end]` of a file plus the bytes already written to disk.
/// Serialised as `[start, end, done]` (same as the Python implementation's state file).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "[u64; 3]", into = "[u64; 3]")]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

impl From<[u64; 3]> for Segment {
    fn from(a: [u64; 3]) -> Self {
        Self { start: a[0], end: a[1], done: a[2] }
    }
}

impl From<Segment> for [u64; 3] {
    fn from(s: Segment) -> Self {
        [s.start, s.end, s.done]
    }
}

impl Segment {
    pub fn len(&self) -> u64 {
        self.end + 1 - self.start
    }
    pub fn remaining(&self) -> u64 {
        self.len().saturating_sub(self.done)
    }
}

/// Split `size` bytes into at most `connections` ranges of at least `min_segment` bytes.
pub fn plan_segments(size: u64, connections: usize, min_segment: u64) -> Vec<Segment> {
    if size == 0 {
        return Vec::new();
    }
    let n = (connections.max(1) as u64).min((size / min_segment.max(1)).max(1));
    let step = size.div_ceil(n);
    (0..n)
        .map_while(|i| {
            let start = i * step;
            (start < size).then(|| Segment { start, end: (start + step).min(size) - 1, done: 0 })
        })
        .collect()
}

/// Segments must tile `[0, size)` exactly, otherwise the persisted plan is stale.
fn segments_valid(segs: &[Segment], size: u64) -> bool {
    if size == 0 || segs.is_empty() {
        return false;
    }
    let mut next = 0;
    for s in segs {
        if s.start != next || s.end < s.start || s.done > s.len() {
            return false;
        }
        next = s.end + 1;
    }
    next == size
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileState {
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub complete: bool,
}

impl FileState {
    fn downloaded(&self) -> u64 {
        if self.complete {
            self.size
        } else {
            self.segments.iter().map(|s| s.done.min(s.len())).sum()
        }
    }
}

fn default_revision() -> String {
    "main".into()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub kind: Kind,
    pub repo: String,
    #[serde(default = "default_revision")]
    pub revision: String,
    #[serde(default)]
    pub files: Vec<FileState>,
    #[serde(default)]
    pub dest: String,
    pub status: Status,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub finished_at: Option<f64>,
    #[serde(default)]
    pub ollama_total: u64,
    #[serde(default)]
    pub ollama_done: u64,
    #[serde(default)]
    pub ollama_status: String,
    /// Incremented every time a worker is started or invalidated (pause/cancel/demote), so a
    /// stale worker can never touch the state of a newer run.
    #[serde(skip)]
    run_id: u64,
    #[serde(skip)]
    samples: VecDeque<(Instant, u64)>,
}

#[derive(Debug, Serialize)]
pub struct FileView {
    pub path: String,
    pub size: u64,
    pub downloaded: u64,
    pub complete: bool,
    pub connections: usize,
}

/// Public JSON shape (`Download` in frontend/src/lib/api.ts).
#[derive(Debug, Serialize)]
pub struct TaskView {
    pub id: String,
    pub kind: Kind,
    pub repo: String,
    pub revision: String,
    pub dest: String,
    pub status: Status,
    pub error: Option<String>,
    pub created_at: f64,
    pub finished_at: Option<f64>,
    pub total: u64,
    pub downloaded: u64,
    pub speed: f64,
    pub eta: Option<f64>,
    pub ollama_total: u64,
    pub ollama_done: u64,
    pub ollama_status: String,
    pub files: Vec<FileView>,
}

impl Task {
    fn total(&self) -> u64 {
        match self.kind {
            Kind::Ollama => self.ollama_total,
            Kind::Hf => self.files.iter().map(|f| f.size).sum(),
        }
    }

    fn downloaded(&self) -> u64 {
        match self.kind {
            Kind::Ollama => self.ollama_done,
            Kind::Hf => self.files.iter().map(FileState::downloaded).sum(),
        }
    }

    fn sample(&mut self, now: Instant) {
        let bytes = self.downloaded();
        if self.samples.back().is_some_and(|(t, _)| now.duration_since(*t) < Duration::from_millis(200)) {
            self.samples.pop_back();
        }
        self.samples.push_back((now, bytes));
        while self.samples.len() > 2 && self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > SPEED_WINDOW) {
            self.samples.pop_front();
        }
    }

    /// Bytes/s over the sliding window (0 unless actively downloading).
    fn speed(&self) -> f64 {
        if self.status != Status::Downloading || self.samples.len() < 2 {
            return 0.0;
        }
        let (t0, b0) = self.samples[0];
        let (t1, b1) = self.samples[self.samples.len() - 1];
        let dt = t1.duration_since(t0).as_secs_f64();
        if dt <= 0.0 {
            0.0
        } else {
            b1.saturating_sub(b0) as f64 / dt
        }
    }

    pub fn view(&self) -> TaskView {
        let total = self.total();
        let downloaded = self.downloaded();
        let speed = self.speed();
        TaskView {
            id: self.id.clone(),
            kind: self.kind,
            repo: self.repo.clone(),
            revision: self.revision.clone(),
            dest: self.dest.clone(),
            status: self.status,
            error: self.error.clone(),
            created_at: self.created_at,
            finished_at: self.finished_at,
            total,
            downloaded,
            speed,
            eta: (speed > 0.0).then(|| total.saturating_sub(downloaded) as f64 / speed),
            ollama_total: self.ollama_total,
            ollama_done: self.ollama_done,
            ollama_status: self.ollama_status.clone(),
            files: self
                .files
                .iter()
                .map(|f| FileView {
                    path: f.path.clone(),
                    size: f.size,
                    downloaded: f.downloaded(),
                    complete: f.complete,
                    connections: f.segments.len(),
                })
                .collect(),
        }
    }
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

// ---------------------------------------------------------------------------------------------
// Bandwidth limiter
// ---------------------------------------------------------------------------------------------

const MIN_BURST: f64 = 16.0 * 1024.0;

/// Token bucket. Burst is a quarter of a second of traffic. Taking more than the bucket holds
/// puts it into debt, which later callers repay by waiting, so the long-run rate is exact even
/// when a single chunk is larger than the burst.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
    rate: Option<u64>,
}

fn burst_for(rate: u64) -> f64 {
    (rate as f64 / 4.0).max(MIN_BURST)
}

impl Bucket {
    fn new(now: Instant) -> Self {
        Self { tokens: 0.0, last: now, rate: None }
    }

    /// `Ok` if `n` bytes may be sent now, else how long to wait before asking again.
    fn take(&mut self, now: Instant, rate: Option<u64>, n: u64) -> Result<(), Duration> {
        let Some(rate) = rate else {
            self.rate = None;
            return Ok(());
        };
        let burst = burst_for(rate);
        if self.rate != Some(rate) {
            // Rate just appeared or changed: start full, never above the new burst.
            self.tokens = if self.rate.is_none() { burst } else { self.tokens.min(burst) };
            self.rate = Some(rate);
        } else {
            let dt = now.saturating_duration_since(self.last).as_secs_f64();
            self.tokens = (self.tokens + dt * rate as f64).min(burst);
        }
        self.last = now;
        let need = (n as f64).min(burst);
        if self.tokens >= need {
            self.tokens -= n as f64;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((need - self.tokens) / rate as f64).max(Duration::from_millis(1)))
        }
    }
}

/// Global download bandwidth limiter shared by every segment of every task. The rate is read
/// from the mode profile on each call, so a mode switch applies to transfers already running.
pub struct RateLimiter {
    rx: watch::Receiver<ModeProfile>,
    bucket: Mutex<Bucket>,
}

impl RateLimiter {
    pub fn new(rx: watch::Receiver<ModeProfile>) -> Self {
        Self { rx, bucket: Mutex::new(Bucket::new(Instant::now())) }
    }

    /// Wait until `n` bytes may be consumed. Unlimited mode returns immediately.
    pub async fn acquire(&self, n: usize) {
        loop {
            let rate = self.rx.borrow().download_bytes_per_sec;
            if rate.is_none() {
                return;
            }
            let wait = self.bucket.lock().take(Instant::now(), rate, n as u64);
            match wait {
                Ok(()) => return,
                // Re-check the rate at least every 100 ms so lifting a limit is felt quickly.
                Err(w) => tokio::time::sleep(w.min(Duration::from_millis(100))).await,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Ollama progress aggregation
// ---------------------------------------------------------------------------------------------

#[derive(Default, Debug)]
struct OllamaAgg {
    layers: HashMap<String, (u64, u64)>,
    status: String,
    success: bool,
}

impl OllamaAgg {
    /// Feed one NDJSON line. `Err` carries Ollama's own error message.
    fn feed(&mut self, line: &str) -> Result<(), String> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        let ev: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("Ollama の応答を解釈できません: {e}"))?;
        if let Some(err) = ev.get("error") {
            return Err(err.as_str().map(str::to_owned).unwrap_or_else(|| err.to_string()));
        }
        if let Some(s) = ev.get("status").and_then(|s| s.as_str()) {
            self.status = s.to_string();
            if s == "success" {
                self.success = true;
            }
        }
        if let (Some(digest), Some(total)) = (ev.get("digest").and_then(|d| d.as_str()), ev.get("total").and_then(|t| t.as_u64())) {
            if total > 0 {
                let done = ev.get("completed").and_then(|c| c.as_u64()).unwrap_or(0);
                self.layers.insert(digest.to_string(), (total, done));
            }
        }
        Ok(())
    }
    fn total(&self) -> u64 {
        self.layers.values().map(|v| v.0).sum()
    }
    fn done(&self) -> u64 {
        self.layers.values().map(|v| v.1.min(v.0)).sum()
    }
}

// ---------------------------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------------------------

type SettingsFn = Arc<dyn Fn() -> Settings + Send + Sync>;
/// `(repo, path, revision) -> URL`. Injectable so tests can point at a local server.
pub type UrlFn = Arc<dyn Fn(&str, &str, &str) -> String + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Tuning {
    pub min_segment: u64,
    pub write_buf: usize,
    pub flush_interval: Duration,
    /// Base unit of the retry back-off (`unit * 2^attempt`, capped at `unit * 30`).
    pub backoff_unit: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self { min_segment: MIN_SEGMENT, write_buf: WRITE_BUF, flush_interval: FLUSH_INTERVAL, backoff_unit: Duration::from_secs(1) }
    }
}

fn backoff(attempt: u32, unit: Duration) -> Duration {
    (unit * 2u32.saturating_pow(attempt.min(10))).min(unit * 30)
}

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, Task>,
    handles: HashMap<String, JoinHandle<()>>,
}

pub struct DownloadManager {
    inner: Mutex<Inner>,
    settings: SettingsFn,
    mode_rx: watch::Receiver<ModeProfile>,
    limiter: RateLimiter,
    wake: Notify,
    state_path: PathBuf,
    persist_lock: Mutex<()>,
    url_fn: Option<UrlFn>,
    tuning: Tuning,
}

/// A file request of `POST /api/downloads/hf`.
#[derive(Debug, Deserialize)]
pub struct NewFile {
    pub path: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub sha256: Option<String>,
}

/// Relative path without `..`, absolute or empty components.
fn safe_rel(p: &str) -> bool {
    !p.is_empty() && !p.contains(['\0', '\\']) && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

fn part_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    final_path.with_file_name(name)
}

enum SegErr {
    /// Not worth retrying (auth failure, bad Range support, disk error ...).
    Fatal(String),
    Retry(String),
}

impl DownloadManager {
    /// Load persisted tasks from `~/.llm-harness/downloads.json`. Tasks that were running when
    /// the harness stopped go back to the queue (auto-resume).
    pub fn load(settings: Arc<SettingsStore>, mode_rx: watch::Receiver<ModeProfile>) -> Arc<Self> {
        Self::build(Arc::new(move || settings.get()), mode_rx, HARNESS_HOME.join("downloads.json"), None, Tuning::default())
    }

    /// Fully injectable constructor (tests): settings source, state file, URL builder, tuning.
    pub fn build(
        settings: SettingsFn,
        mode_rx: watch::Receiver<ModeProfile>,
        state_path: PathBuf,
        url_fn: Option<UrlFn>,
        tuning: Tuning,
    ) -> Arc<Self> {
        let mut tasks = HashMap::new();
        for mut t in read_json_file::<Vec<Task>>(&state_path).unwrap_or_default() {
            if t.status.running() {
                t.status = Status::Queued;
            }
            tasks.insert(t.id.clone(), t);
        }
        Arc::new(Self {
            inner: Mutex::new(Inner { tasks, handles: HashMap::new() }),
            settings,
            limiter: RateLimiter::new(mode_rx.clone()),
            mode_rx,
            wake: Notify::new(),
            state_path,
            persist_lock: Mutex::new(()),
            url_fn,
            tuning,
        })
    }

    /// Flush state to disk.
    pub fn persist(&self) {
        let _serial = self.persist_lock.lock();
        let snapshot = {
            let g = self.inner.lock();
            let mut v: Vec<&Task> = g.tasks.values().collect();
            v.sort_by(|a, b| a.created_at.total_cmp(&b.created_at));
            serde_json::to_value(v)
        };
        let result = snapshot.map_err(anyhow::Error::from).and_then(|v| {
            if let Some(dir) = self.state_path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            write_json_file(&self.state_path, &v)
        });
        if let Err(e) = result {
            tracing::warn!("downloads.json を保存できません: {e:#}");
        }
    }

    // ----------------------------------------------------------------------------- public API

    /// All tasks, newest first.
    pub fn list(&self) -> Vec<TaskView> {
        let g = self.inner.lock();
        let mut v: Vec<&Task> = g.tasks.values().collect();
        v.sort_by(|a, b| b.created_at.total_cmp(&a.created_at));
        v.into_iter().map(Task::view).collect()
    }

    pub fn get(&self, id: &str) -> Option<TaskView> {
        self.inner.lock().tasks.get(id).map(Task::view)
    }

    pub fn add_hf(&self, repo: &str, files: Vec<NewFile>, revision: Option<String>) -> Result<TaskView, String> {
        let repo = repo.trim().trim_matches('/').to_string();
        if !safe_rel(&repo) {
            return Err("repo が不正です".into());
        }
        if files.is_empty() {
            return Err("ファイルが指定されていません".into());
        }
        let revision = revision.map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).unwrap_or_else(default_revision);
        let mut states = Vec::with_capacity(files.len());
        for f in files {
            if !safe_rel(&f.path) {
                return Err(format!("ファイルパスが不正です: {}", f.path));
            }
            if states.iter().any(|s: &FileState| s.path == f.path) {
                continue;
            }
            let sha256 = f.sha256.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
            states.push(FileState { path: f.path, size: f.size.unwrap_or(0), sha256, segments: Vec::new(), complete: false });
        }
        let dest = (self.settings)().models_dir().join(&repo).to_string_lossy().into_owned();
        let view = {
            let mut g = self.inner.lock();
            // Two tasks sharing one `.part` file would corrupt each other.
            let clash = g.tasks.values().find(|t| {
                t.kind == Kind::Hf && t.dest == dest && t.status.unfinished() && t.files.iter().any(|f| !f.complete && states.iter().any(|s| s.path == f.path))
            });
            if clash.is_some() {
                return Err("同じファイルのダウンロードが既にあります (完了/キャンセルしてから再度追加してください)".into());
            }
            let task = Task {
                id: new_id(),
                kind: Kind::Hf,
                repo,
                revision,
                files: states,
                dest,
                status: Status::Queued,
                error: None,
                created_at: now_secs(),
                finished_at: None,
                ollama_total: 0,
                ollama_done: 0,
                ollama_status: String::new(),
                run_id: 0,
                samples: VecDeque::new(),
            };
            let view = task.view();
            g.tasks.insert(task.id.clone(), task);
            view
        };
        self.persist();
        self.wake.notify_one();
        Ok(view)
    }

    pub fn add_ollama(&self, model: &str) -> Result<TaskView, String> {
        let model = model.trim().to_string();
        if model.is_empty() {
            return Err("モデル名が指定されていません".into());
        }
        let view = {
            let mut g = self.inner.lock();
            let task = Task {
                id: new_id(),
                kind: Kind::Ollama,
                repo: model,
                revision: default_revision(),
                files: Vec::new(),
                dest: "ollama".into(),
                status: Status::Queued,
                error: None,
                created_at: now_secs(),
                finished_at: None,
                ollama_total: 0,
                ollama_done: 0,
                ollama_status: String::new(),
                run_id: 0,
                samples: VecDeque::new(),
            };
            let view = task.view();
            g.tasks.insert(task.id.clone(), task);
            view
        };
        self.persist();
        self.wake.notify_one();
        Ok(view)
    }

    /// Stop the worker immediately; segments stay so `resume` continues where it left off.
    /// Returns false for an unknown id.
    pub fn pause(&self, id: &str) -> bool {
        {
            let mut g = self.inner.lock();
            let Some(t) = g.tasks.get_mut(id) else { return false };
            if matches!(t.status, Status::Queued | Status::Downloading | Status::Verifying) {
                t.status = Status::Paused;
                t.run_id += 1;
                t.samples.clear();
                if let Some(h) = g.handles.remove(id) {
                    h.abort();
                }
            }
        }
        self.persist();
        true
    }

    pub fn resume(&self, id: &str) -> bool {
        {
            let mut g = self.inner.lock();
            let Some(t) = g.tasks.get_mut(id) else { return false };
            if matches!(t.status, Status::Paused | Status::Failed | Status::Cancelled) {
                t.status = Status::Queued;
                t.error = None;
                t.finished_at = None;
            }
        }
        self.persist();
        self.wake.notify_one();
        true
    }

    /// Stop immediately and throw away partial data.
    pub fn cancel(&self, id: &str) -> bool {
        {
            let mut g = self.inner.lock();
            let Some(t) = g.tasks.get_mut(id) else { return false };
            if t.status == Status::Completed {
                return true;
            }
            Self::cancel_locked(t);
            if let Some(h) = g.handles.remove(id) {
                h.abort();
            }
        }
        self.persist();
        self.wake.notify_one();
        true
    }

    fn cancel_locked(t: &mut Task) {
        t.status = Status::Cancelled;
        t.run_id += 1;
        t.samples.clear();
        if t.kind == Kind::Hf {
            for f in t.files.iter_mut().filter(|f| !f.complete) {
                let _ = std::fs::remove_file(part_path(&Path::new(&t.dest).join(&f.path)));
                f.segments.clear();
            }
        }
    }

    /// Remove a task from the list (partial data of unfinished tasks is deleted too).
    pub fn remove(&self, id: &str) {
        {
            let mut g = self.inner.lock();
            if let Some(t) = g.tasks.get_mut(id) {
                if t.status != Status::Completed && t.status != Status::Cancelled {
                    Self::cancel_locked(t);
                }
            }
            g.tasks.remove(id);
            if let Some(h) = g.handles.remove(id) {
                h.abort();
            }
        }
        self.persist();
        self.wake.notify_one();
    }

    // -------------------------------------------------------------------------------- scheduler

    /// Scheduler loop; never returns.
    pub async fn run(self: Arc<Self>) {
        let mut rx = self.mode_rx.clone();
        let mut mode_alive = true;
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_persist = Instant::now();
        loop {
            let active = self.schedule();
            if active && last_persist.elapsed() >= PERSIST_EVERY {
                self.persist();
                last_persist = Instant::now();
            }
            tokio::select! {
                _ = tick.tick() => {}
                _ = self.wake.notified() => {}
                r = rx.changed(), if mode_alive => { mode_alive = r.is_ok(); }
            }
        }
    }

    /// One scheduling pass: reap finished workers, apply the live concurrency limit, start
    /// queued tasks (oldest first), take speed samples. Returns whether anything is running.
    fn schedule(self: &Arc<Self>) -> bool {
        let limit = self.mode_rx.borrow().max_concurrent_downloads.max(1);
        let now = Instant::now();
        let mut changed = false;
        let active;
        {
            let mut g = self.inner.lock();
            let Inner { tasks, handles } = &mut *g;
            handles.retain(|_, h| !h.is_finished());
            // A worker that died without recording a result (panic) must not leave a ghost task.
            for t in tasks.values_mut() {
                if t.status.running() && !handles.contains_key(&t.id) {
                    t.status = Status::Failed;
                    t.error = Some("内部エラー: ダウンロード処理が異常終了しました".into());
                    changed = true;
                }
            }
            // Concurrency dropped (e.g. switch to shared mode): send the newest surplus tasks
            // back to the queue. Their segments are kept, so nothing is lost.
            if handles.len() > limit {
                let mut running: Vec<(f64, String)> =
                    handles.keys().filter_map(|id| tasks.get(id).map(|t| (t.created_at, id.clone()))).collect();
                running.sort_by(|a, b| b.0.total_cmp(&a.0));
                let mut excess = handles.len() - limit;
                for (_, id) in running {
                    if excess == 0 {
                        break;
                    }
                    let t = tasks.get_mut(&id).expect("running task exists");
                    if t.status == Status::Downloading {
                        t.status = Status::Queued;
                        t.run_id += 1;
                        t.samples.clear();
                        if let Some(h) = handles.remove(&id) {
                            h.abort();
                        }
                        excess -= 1;
                        changed = true;
                    }
                }
            }
            if handles.len() < limit {
                let mut queued: Vec<(f64, String)> =
                    tasks.values().filter(|t| t.status == Status::Queued && !handles.contains_key(&t.id)).map(|t| (t.created_at, t.id.clone())).collect();
                queued.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                for (_, id) in queued {
                    if handles.len() >= limit {
                        break;
                    }
                    let t = tasks.get_mut(&id).expect("queued task exists");
                    t.status = Status::Downloading;
                    t.error = None;
                    t.run_id += 1;
                    t.samples.clear();
                    let (run, mgr, tid) = (t.run_id, self.clone(), id.clone());
                    handles.insert(id, tokio::spawn(async move { mgr.execute(tid, run).await }));
                    changed = true;
                }
            }
            let mut any = false;
            for t in tasks.values_mut() {
                if t.status.running() {
                    t.sample(now);
                    any = true;
                } else if !t.samples.is_empty() {
                    t.samples.clear();
                }
            }
            active = any;
        }
        if changed {
            self.persist();
        }
        active
    }

    // ------------------------------------------------------------------------------- execution

    /// Run `f` on the task only if it still belongs to run `run`.
    fn with_task<R>(&self, id: &str, run: u64, f: impl FnOnce(&mut Task) -> R) -> Option<R> {
        let mut g = self.inner.lock();
        let t = g.tasks.get_mut(id)?;
        (t.run_id == run).then(|| f(t))
    }

    async fn execute(self: Arc<Self>, id: String, run: u64) {
        let Some(kind) = self.with_task(&id, run, |t| t.kind) else { return };
        let res = match kind {
            Kind::Hf => self.run_hf(&id, run).await,
            Kind::Ollama => self.run_ollama(&id, run).await,
        };
        self.with_task(&id, run, |t| match res {
            Ok(()) => {
                t.status = Status::Completed;
                t.error = None;
                t.finished_at = Some(now_secs());
            }
            Err(e) => {
                tracing::warn!("download {} failed: {e:#}", t.repo);
                t.status = Status::Failed;
                t.error = Some(format!("{e:#}"));
            }
        });
        self.persist();
        self.wake.notify_one();
    }

    async fn run_hf(self: &Arc<Self>, id: &str, run: u64) -> anyhow::Result<()> {
        let (dest, count) = self.with_task(id, run, |t| (t.dest.clone(), t.files.len())).ok_or_else(superseded)?;
        tokio::fs::create_dir_all(&dest).await?;
        for fi in 0..count {
            let done = self.with_task(id, run, |t| t.files[fi].complete).ok_or_else(superseded)?;
            if !done {
                self.fetch_file(id, run, fi).await?;
            }
        }
        Ok(())
    }

    fn url_for(&self, repo: &str, path: &str, revision: &str) -> String {
        match &self.url_fn {
            Some(f) => f(repo, path, revision),
            None => hf::resolve_url(&(self.settings)().hf_endpoint, repo, path, revision),
        }
    }

    async fn fetch_file(self: &Arc<Self>, id: &str, run: u64, fi: usize) -> anyhow::Result<()> {
        let (repo, revision, dest, file) = self
            .with_task(id, run, |t| (t.repo.clone(), t.revision.clone(), PathBuf::from(&t.dest), t.files[fi].clone()))
            .ok_or_else(superseded)?;
        let url = self.url_for(&repo, &file.path, &revision);
        let final_path = dest.join(&file.path);
        let part = part_path(&final_path);
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut size = file.size;
        if size == 0 {
            let mut attempt = 0u32;
            size = loop {
                match hf::remote_size(&(self.settings)(), &url).await {
                    Ok(n) => break n,
                    // Auth / not-found errors are final; anything else gets a few retries.
                    Err(e) if attempt >= 4 || e.to_string().starts_with("HTTP 40") => return Err(e),
                    Err(_) => {
                        attempt += 1;
                        tokio::time::sleep(backoff(attempt, self.tuning.backoff_unit)).await;
                    }
                }
            };
            self.with_task(id, run, |t| t.files[fi].size = size).ok_or_else(superseded)?;
        }
        if size == 0 {
            // Genuinely empty file.
            tokio::fs::write(&final_path, b"").await?;
            self.with_task(id, run, |t| t.files[fi].complete = true).ok_or_else(superseded)?;
            return Ok(());
        }

        let part_ok = std::fs::metadata(&part).map(|m| m.len() == size).unwrap_or(false);
        let mut segments = file.segments;
        if !part_ok || !segments_valid(&segments, size) {
            // (Re)plan with the connection count of the *current* profile.
            let n = self.mode_rx.borrow().connections_per_file.max(1);
            segments = plan_segments(size, n, self.tuning.min_segment);
            let f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&part)?;
            f.set_len(size)?;
            let planned = segments.clone();
            self.with_task(id, run, |t| t.files[fi].segments = planned).ok_or_else(superseded)?;
            self.persist();
        }

        if segments.iter().any(|s| s.remaining() > 0) {
            let handle = Arc::new(std::fs::OpenOptions::new().read(true).write(true).open(&part)?);
            let mut set: JoinSet<anyhow::Result<()>> = JoinSet::new();
            for (si, seg) in segments.iter().enumerate() {
                if seg.remaining() == 0 {
                    continue;
                }
                let ctx = SegCtx { mgr: self.clone(), id: id.to_string(), run, fi, si, url: url.clone(), file: handle.clone() };
                set.spawn(ctx.fetch());
            }
            // First failure aborts the siblings (dropping the JoinSet also aborts them if this
            // future itself is aborted by pause/cancel).
            while let Some(res) = set.join_next().await {
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        set.abort_all();
                        return Err(e);
                    }
                    Err(e) => {
                        set.abort_all();
                        return Err(anyhow!("内部エラー: {e}"));
                    }
                }
            }
        }

        let complete = self.with_task(id, run, |t| t.files[fi].downloaded() >= size).ok_or_else(superseded)?;
        if !complete {
            bail!("ダウンロードが不完全です ({})", file.path);
        }

        if let Some(expected) = file.sha256.filter(|s| !s.is_empty()) {
            self.with_task(id, run, |t| t.status = Status::Verifying).ok_or_else(superseded)?;
            self.persist();
            let cancel = Arc::new(AtomicBool::new(false));
            let _guard = CancelOnDrop(cancel.clone());
            let p = part.clone();
            let digest = tokio::task::spawn_blocking(move || sha256_file(&p, &cancel))
                .await
                .map_err(|e| anyhow!("内部エラー: {e}"))??
                .ok_or_else(|| anyhow!("検証が中断されました"))?;
            if !digest.eq_ignore_ascii_case(&expected) {
                let _ = std::fs::remove_file(&part);
                self.with_task(id, run, |t| t.files[fi].segments.clear());
                self.persist();
                bail!("sha256 不一致 ({}) — 再開すると最初から取り直します", file.path);
            }
            self.with_task(id, run, |t| t.status = Status::Downloading).ok_or_else(superseded)?;
        }

        tokio::fs::rename(&part, &final_path).await?;
        self.with_task(id, run, |t| t.files[fi].complete = true).ok_or_else(superseded)?;
        self.persist();
        Ok(())
    }

    async fn run_ollama(self: &Arc<Self>, id: &str, run: u64) -> anyhow::Result<()> {
        let model = self.with_task(id, run, |t| t.repo.clone()).ok_or_else(superseded)?;
        let base = (self.settings)().ollama_url.trim_end_matches('/').to_string();
        let resp = ollama_client()
            .post(format!("{base}/api/pull"))
            .json(&serde_json::json!({ "model": model, "stream": true }))
            .send()
            .await
            .map_err(|e| anyhow!("Ollama に接続できません ({base}): {e}"))?;
        if !resp.status().is_success() {
            let code = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            bail!("Ollama HTTP {code}: {}", body.chars().take(200).collect::<String>());
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut agg = OllamaAgg::default();
        let apply = |agg: &mut OllamaAgg, line: &[u8]| -> anyhow::Result<()> {
            agg.feed(&String::from_utf8_lossy(line)).map_err(|e| anyhow!("Ollama: {e}"))?;
            let (status, total, done) = (agg.status.clone(), agg.total(), agg.done());
            self.with_task(id, run, |t| {
                t.ollama_status = status;
                t.ollama_total = total;
                t.ollama_done = done;
            })
            .ok_or_else(superseded)?;
            Ok(())
        };
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.map_err(|e| anyhow!("Ollama との接続が切れました: {e}"))?);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                apply(&mut agg, &line)?;
            }
        }
        if !buf.is_empty() {
            apply(&mut agg, &buf)?;
        }
        if !agg.success {
            bail!("Ollama の pull が途中で終了しました (最後の状態: {})", agg.status);
        }
        self.with_task(id, run, |t| t.ollama_done = t.ollama_total).ok_or_else(superseded)?;
        Ok(())
    }
}

fn superseded() -> anyhow::Error {
    anyhow!("中断されました")
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn ollama_client() -> &'static reqwest::Client {
    static C: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent(hf::USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("build ollama client")
    });
    &C
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// SHA-256 of a file; `None` when `cancel` was raised mid-way.
fn sha256_file(path: &Path, cancel: &AtomicBool) -> io::Result<Option<String>> {
    use std::io::Read;
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; HASH_BUF];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(Some(format!("{:x}", h.finalize())))
}

/// Positioned write; offloaded with `block_in_place` on the multi-thread runtime so a slow disk
/// never stalls other async tasks.
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(RuntimeFlavor::CurrentThread) | Err(_) => file.write_all_at(buf, offset),
        Ok(_) => tokio::task::block_in_place(|| file.write_all_at(buf, offset)),
    }
}

// ---------------------------------------------------------------------------------------------
// Segment worker
// ---------------------------------------------------------------------------------------------

struct SegCtx {
    mgr: Arc<DownloadManager>,
    id: String,
    run: u64,
    fi: usize,
    si: usize,
    url: String,
    file: Arc<File>,
}

impl SegCtx {
    fn segment(&self) -> Option<Segment> {
        self.mgr.with_task(&self.id, self.run, |t| t.files.get(self.fi).and_then(|f| f.segments.get(self.si)).copied()).flatten()
    }

    /// Download the segment to completion, retrying with exponential back-off.
    async fn fetch(self) -> anyhow::Result<()> {
        let mut attempts = 0u32;
        let mut last_done = 0u64;
        loop {
            let seg = self.segment().ok_or_else(superseded)?;
            if seg.remaining() == 0 {
                return Ok(());
            }
            if seg.done > last_done {
                attempts = 0;
                last_done = seg.done;
            }
            match self.attempt(seg).await {
                Ok(()) => {}
                Err(SegErr::Fatal(m)) => bail!("{m}"),
                Err(SegErr::Retry(m)) => {
                    attempts += 1;
                    if attempts > MAX_RETRIES {
                        bail!("{m} (リトライ上限に達しました)");
                    }
                    tracing::debug!("segment {}: {m}, retry {attempts}", self.si);
                    tokio::time::sleep(backoff(attempts, self.mgr.tuning.backoff_unit)).await;
                }
            }
        }
    }

    /// One HTTP request covering the rest of the segment.
    async fn attempt(&self, seg: Segment) -> Result<(), SegErr> {
        let pos = seg.start + seg.done;
        let settings = (self.mgr.settings)();
        let rb = hf::authorize(hf::download_client().get(&self.url).header(RANGE, format!("bytes={pos}-{}", seg.end)), &settings);
        let mut resp = rb.send().await.map_err(|e| SegErr::Retry(format!("接続エラー: {e}")))?;
        let status = resp.status().as_u16();
        match status {
            206 => {
                let starts_ok = resp
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes "))
                    .and_then(|v| v.split(['-', '/']).next())
                    .and_then(|v| v.parse::<u64>().ok())
                    .is_none_or(|s| s == pos);
                if !starts_ok {
                    return Err(SegErr::Fatal("サーバーが想定外の Range を返しました".into()));
                }
            }
            200 if pos == 0 => {}
            200 => return Err(SegErr::Fatal("サーバーが Range リクエストを無視しました (HTTP 200)".into())),
            401 | 403 | 404 => return Err(SegErr::Fatal(format!("HTTP {status} (gated repo なら HF token を設定)"))),
            408 | 429 | 500..=599 => return Err(SegErr::Retry(format!("HTTP {status}"))),
            _ => return Err(SegErr::Fatal(format!("HTTP {status}"))),
        }

        let tune = &self.mgr.tuning;
        let mut buf: Vec<u8> = Vec::with_capacity(tune.write_buf + 128 * 1024);
        let mut offset = pos;
        let mut remaining = seg.end + 1 - pos;
        let mut last_flush = Instant::now();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    let take = (chunk.len() as u64).min(remaining) as usize;
                    self.mgr.limiter.acquire(take).await;
                    buf.extend_from_slice(&chunk[..take]);
                    remaining -= take as u64;
                    if remaining == 0 || buf.len() >= tune.write_buf || last_flush.elapsed() >= tune.flush_interval {
                        self.flush(&mut buf, &mut offset)?;
                        last_flush = Instant::now();
                    }
                    if remaining == 0 {
                        return Ok(());
                    }
                }
                Ok(None) => {
                    self.flush(&mut buf, &mut offset)?;
                    return if remaining == 0 { Ok(()) } else { Err(SegErr::Retry("接続が途中で切断されました".into())) };
                }
                Err(e) => {
                    // Keep what already arrived so the retry resumes after it.
                    self.flush(&mut buf, &mut offset)?;
                    return Err(SegErr::Retry(format!("受信エラー: {e}")));
                }
            }
        }
    }

    /// Write the buffer to disk, then (and only then) advance the persisted `done` counter.
    fn flush(&self, buf: &mut Vec<u8>, offset: &mut u64) -> Result<(), SegErr> {
        if buf.is_empty() {
            return Ok(());
        }
        write_at(&self.file, buf, *offset).map_err(|e| SegErr::Fatal(format!("書き込みエラー: {e}")))?;
        let n = buf.len() as u64;
        buf.clear();
        *offset += n;
        let advanced = self
            .mgr
            .with_task(&self.id, self.run, |t| match t.files.get_mut(self.fi).and_then(|f| f.segments.get_mut(self.si)) {
                Some(seg) => {
                    seg.done = (seg.done + n).min(seg.len());
                    true
                }
                None => false,
            })
            .unwrap_or(false);
        if advanced {
            Ok(())
        } else {
            Err(SegErr::Fatal("中断されました".into()))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------------------------

fn default_true() -> bool {
    true
}
fn default_limit() -> u32 {
    30
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "default_true")]
    gguf: bool,
    #[serde(default = "default_limit")]
    limit: u32,
}

#[derive(Deserialize)]
struct FilesQuery {
    #[serde(default = "default_revision")]
    revision: String,
}

#[derive(Deserialize)]
struct HfDownloadReq {
    repo: String,
    #[serde(default)]
    files: Vec<NewFile>,
    #[serde(default)]
    revision: Option<String>,
}

#[derive(Deserialize)]
struct OllamaPullReq {
    model: String,
}

async fn hf_search(State(s): State<AppState>, Query(q): Query<SearchQuery>) -> ApiResult<Vec<HfRepo>> {
    let r = hf::search(&s.settings.get(), &q.q, q.gguf, q.limit.clamp(1, 100)).await;
    r.map(Json).map_err(|e| ApiError::bad_gateway(format!("{e:#}")))
}

async fn hf_files(State(s): State<AppState>, AxPath(repo): AxPath<String>, Query(q): Query<FilesQuery>) -> ApiResult<Vec<HfFile>> {
    let r = hf::files(&s.settings.get(), repo.trim_matches('/'), &q.revision).await;
    r.map(Json).map_err(|e| ApiError::bad_gateway(format!("{e:#}")))
}

async fn list_downloads(State(s): State<AppState>) -> Json<Vec<TaskView>> {
    Json(s.downloads.list())
}

async fn add_hf(State(s): State<AppState>, Json(req): Json<HfDownloadReq>) -> ApiResult<TaskView> {
    s.downloads.add_hf(&req.repo, req.files, req.revision).map(Json).map_err(ApiError::bad_request)
}

async fn add_ollama(State(s): State<AppState>, Json(req): Json<OllamaPullReq>) -> ApiResult<TaskView> {
    s.downloads.add_ollama(&req.model).map(Json).map_err(ApiError::bad_request)
}

async fn download_action(State(s): State<AppState>, AxPath((id, action)): AxPath<(String, String)>) -> ApiResult<serde_json::Value> {
    let found = match action.as_str() {
        "pause" => s.downloads.pause(&id),
        "resume" => s.downloads.resume(&id),
        "cancel" => s.downloads.cancel(&id),
        _ => return Err(ApiError::bad_request("bad action")),
    };
    if found {
        ok()
    } else {
        Err(ApiError::not_found("not found"))
    }
}

async fn get_download(State(s): State<AppState>, AxPath(id): AxPath<String>) -> ApiResult<TaskView> {
    s.downloads.get(&id).map(Json).ok_or_else(|| ApiError::not_found("not found"))
}

async fn remove_download(State(s): State<AppState>, AxPath(id): AxPath<String>) -> ApiResult<serde_json::Value> {
    s.downloads.remove(&id);
    ok()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/hf/search", get(hf_search))
        .route("/api/hf/files/{*repo}", get(hf_files))
        .route("/api/downloads", get(list_downloads))
        .route("/api/downloads/hf", post(add_hf))
        .route("/api/downloads/ollama", post(add_ollama))
        .route("/api/downloads/{id}/{action}", post(download_action))
        .route("/api/downloads/{id}", get(get_download).delete(remove_download))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;
    use axum::body::{Body, Bytes};
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::Response;
    use std::sync::atomic::AtomicUsize;

    // ---- token bucket -------------------------------------------------------------------

    #[test]
    fn bucket_unlimited_is_free() {
        let mut b = Bucket::new(Instant::now());
        assert!(b.take(Instant::now(), None, u64::MAX / 2).is_ok());
    }

    #[test]
    fn bucket_burst_then_refill() {
        let t0 = Instant::now();
        let rate = 1_000_000u64; // burst = 250_000
        let mut b = Bucket::new(t0);
        assert!(b.take(t0, Some(rate), 250_000).is_ok()); // starts full
        let wait = b.take(t0, Some(rate), 100_000).unwrap_err();
        assert!((wait.as_secs_f64() - 0.1).abs() < 1e-6, "{wait:?}");
        // After 0.1 s exactly enough tokens have been refilled.
        assert!(b.take(t0 + Duration::from_millis(100), Some(rate), 100_000).is_ok());
        // Refill never exceeds the burst.
        let t1 = t0 + Duration::from_secs(60);
        assert!(b.take(t1, Some(rate), 250_000).is_ok());
        assert!(b.take(t1, Some(rate), 1).is_err());
    }

    #[test]
    fn bucket_chunk_larger_than_burst_goes_into_debt() {
        let t0 = Instant::now();
        let rate = 100_000u64; // burst = 25_000
        let mut b = Bucket::new(t0);
        assert!(b.take(t0, Some(rate), 100_000).is_ok()); // allowed: bucket was full
        // Debt of 75_000 bytes: the next caller must wait ~0.75 s + its own need.
        let wait = b.take(t0, Some(rate), 1_000).unwrap_err();
        assert!(wait.as_secs_f64() > 0.7 && wait.as_secs_f64() < 0.9, "{wait:?}");
    }

    #[test]
    fn bucket_follows_rate_changes() {
        let t0 = Instant::now();
        let mut b = Bucket::new(t0);
        assert!(b.take(t0, Some(4_000_000), 1_000_000).is_ok()); // full burst of 1 MB
        // Drop to 100 kB/s: tokens are clamped to the new (25 kB) burst.
        assert!(b.take(t0, Some(100_000), 25_001).is_err());
        // Unlimited again, then limited: starts full again.
        assert!(b.take(t0, None, 1 << 30).is_ok());
        assert!(b.take(t0, Some(100_000), 25_000).is_ok());
    }

    fn profile(conns: usize, concurrent: usize, rate: Option<u64>) -> ModeProfile {
        ModeProfile {
            mode: Mode::Full,
            cpu_set: vec![0],
            physical_cores: 1,
            logical_cpus: 1,
            vram_reserve_mb: 0,
            gpu_power_percent: 100,
            ubatch_cap: 512,
            nice: 0,
            max_concurrent_downloads: concurrent,
            connections_per_file: conns,
            download_bytes_per_sec: rate,
            vmmem_priority: "Normal".into(),
        }
    }

    #[tokio::test]
    async fn limiter_throttles_and_reacts_live() {
        let (tx, rx) = watch::channel(profile(1, 1, Some(4 * 1024 * 1024)));
        let lim = Arc::new(RateLimiter::new(rx));
        // 3 MiB at 4 MiB/s with a 1 MiB burst: roughly 0.5 s.
        let start = Instant::now();
        for _ in 0..48 {
            lim.acquire(64 * 1024).await;
        }
        let took = start.elapsed().as_secs_f64();
        assert!(took > 0.35 && took < 2.0, "took {took}");

        // Lower to 256 KiB/s, then lift the limit while a caller is waiting.
        tx.send_replace(profile(1, 1, Some(256 * 1024)));
        lim.acquire(64 * 1024).await; // drains the (clamped) bucket
        let l2 = lim.clone();
        let waiter = tokio::spawn(async move {
            let s = Instant::now();
            for _ in 0..40 {
                l2.acquire(64 * 1024).await; // 2.5 MiB at 256 KiB/s would be ~10 s
            }
            s.elapsed()
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send_replace(profile(1, 1, None));
        let took = waiter.await.unwrap();
        assert!(took < Duration::from_secs(2), "took {took:?}");
    }

    // ---- planning & small helpers ---------------------------------------------------------

    #[test]
    fn segment_planning() {
        const M: u64 = 1024 * 1024;
        assert!(plan_segments(0, 8, 64 * M).is_empty());
        // Small files get one connection no matter what.
        assert_eq!(plan_segments(10 * M, 8, 64 * M), vec![Segment { start: 0, end: 10 * M - 1, done: 0 }]);
        // 1 GiB with 8 connections: 8 segments of 128 MiB.
        let s = plan_segments(1024 * M, 8, 64 * M);
        assert_eq!(s.len(), 8);
        assert!(s.iter().all(|x| x.len() == 128 * M));
        // Not enough data for all connections: 200 MiB -> 3 segments of >= 64 MiB.
        let s = plan_segments(200 * M, 8, 64 * M);
        assert_eq!(s.len(), 3);
        assert!(s.iter().all(|x| x.len() >= 64 * M));
        // Segments tile the file exactly, also for awkward sizes.
        for (size, n) in [(1u64, 4usize), (1_000_003, 7), (5 * M, 4), (7, 100)] {
            let s = plan_segments(size, n, 1);
            assert!(segments_valid(&s, size), "{size} {n} {s:?}");
            assert_eq!(s.iter().map(Segment::len).sum::<u64>(), size);
        }
    }

    #[test]
    fn stale_segments_are_detected() {
        let mut s = plan_segments(1000, 4, 1);
        assert!(segments_valid(&s, 1000));
        assert!(!segments_valid(&s, 1001));
        s[1].done = 10_000;
        assert!(!segments_valid(&s, 1000));
        assert!(!segments_valid(&[], 1000));
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let u = Duration::from_secs(1);
        assert_eq!(backoff(1, u), Duration::from_secs(2));
        assert_eq!(backoff(3, u), Duration::from_secs(8));
        assert_eq!(backoff(5, u), Duration::from_secs(30));
        assert_eq!(backoff(100, u), Duration::from_secs(30));
    }

    #[test]
    fn router_has_no_route_conflicts() {
        let _ = router(); // axum panics on overlapping routes
    }

    #[test]
    fn path_safety() {
        assert!(safe_rel("org/repo"));
        assert!(safe_rel("sub/model-Q4_K_M.gguf"));
        assert!(!safe_rel(""));
        assert!(!safe_rel("/etc/passwd"));
        assert!(!safe_rel("a/../../b"));
        assert!(!safe_rel("a//b"));
        assert!(!safe_rel("a\\b"));
    }

    #[test]
    fn python_state_file_is_readable() {
        let json = r#"[{"id":"abc","kind":"hf","repo":"o/r","revision":"main","dest":"/m/o/r","status":"downloading",
            "error":null,"created_at":1700000000.5,"finished_at":null,"ollama_total":0,"ollama_done":0,"ollama_status":"",
            "files":[{"path":"a.gguf","size":100,"sha256":null,"segments":[[0,49,50],[50,99,10]]}]}]"#;
        let tasks: Vec<Task> = serde_json::from_str(json).unwrap();
        assert_eq!(tasks[0].files[0].segments[1], Segment { start: 50, end: 99, done: 10 });
        assert_eq!(tasks[0].downloaded(), 60);
        let back = serde_json::to_value(&tasks[0]).unwrap();
        assert_eq!(back["files"][0]["segments"][0], serde_json::json!([0, 49, 50]));
        assert_eq!(back["status"], "downloading");
        assert!(back.get("run_id").is_none());
    }

    #[test]
    fn ollama_aggregation() {
        let mut a = OllamaAgg::default();
        a.feed(r#"{"status":"pulling manifest"}"#).unwrap();
        a.feed(r#"{"status":"pulling aaa","digest":"sha256:aaa","total":100,"completed":10}"#).unwrap();
        a.feed(r#"{"status":"pulling bbb","digest":"sha256:bbb","total":50}"#).unwrap();
        a.feed(r#"{"status":"pulling aaa","digest":"sha256:aaa","total":100,"completed":60}"#).unwrap();
        assert_eq!((a.total(), a.done()), (150, 60));
        assert!(!a.success);
        a.feed(r#"{"status":"success"}"#).unwrap();
        assert!(a.success);
        assert_eq!(a.feed(r#"{"error":"pull model manifest: file does not exist"}"#).unwrap_err(), "pull model manifest: file does not exist");
    }

    // ---- end-to-end against a local range server -----------------------------------------

    struct Srv {
        body: Vec<u8>,
        chunk_delay: Duration,
        fail_first: AtomicUsize,
        hits: AtomicUsize,
    }

    async fn serve(State(s): State<Arc<Srv>>, headers: HeaderMap) -> Response {
        s.hits.fetch_add(1, Ordering::SeqCst);
        if s.fail_first.load(Ordering::SeqCst) > 0 {
            s.fail_first.fetch_sub(1, Ordering::SeqCst);
            return Response::builder().status(StatusCode::SERVICE_UNAVAILABLE).body(Body::empty()).unwrap();
        }
        let total = s.body.len() as u64;
        let (start, end, partial) = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("bytes=")) {
            Some(r) => {
                let (a, b) = r.split_once('-').unwrap();
                let a: u64 = a.parse().unwrap();
                let b: u64 = if b.is_empty() { total - 1 } else { b.parse::<u64>().unwrap().min(total - 1) };
                (a, b, true)
            }
            None => (0, total - 1, false),
        };
        let delay = s.chunk_delay;
        let stream = futures::stream::unfold((s.clone(), start, end + 1), move |(s, pos, stop)| async move {
            if pos >= stop {
                return None;
            }
            tokio::time::sleep(delay).await;
            let e = (pos + 32 * 1024).min(stop);
            let chunk = Bytes::copy_from_slice(&s.body[pos as usize..e as usize]);
            Some((Ok::<_, std::convert::Infallible>(chunk), (s, e, stop)))
        });
        let mut rb = Response::builder().header(header::CONTENT_LENGTH, end + 1 - start);
        rb = if partial {
            rb.status(StatusCode::PARTIAL_CONTENT).header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
        } else {
            rb.status(StatusCode::OK)
        };
        rb.body(Body::from_stream(stream)).unwrap()
    }

    fn random_body(n: usize) -> Vec<u8> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ now_secs().to_bits();
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn hex_sha256(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    struct Env {
        mgr: Arc<DownloadManager>,
        srv: Arc<Srv>,
        tx: watch::Sender<ModeProfile>,
        dir: tempfile::TempDir,
        body_sha: String,
    }

    async fn env(size: usize, chunk_delay: Duration, conns: usize, concurrent: usize) -> Env {
        let body = random_body(size);
        let body_sha = hex_sha256(&body);
        let srv = Arc::new(Srv { body, chunk_delay, fail_first: AtomicUsize::new(0), hits: AtomicUsize::new(0) });
        let app = Router::new().route("/blob/{*p}", get(serve)).with_state(srv.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        let settings = Settings { models_dir: models.to_string_lossy().into(), ..Settings::default() };
        let (tx, rx) = watch::channel(profile(conns, concurrent, None));
        let url_fn: UrlFn = Arc::new(move |repo, path, rev| format!("http://{addr}/blob/{repo}/{rev}/{path}"));
        let tuning = Tuning { min_segment: 1024 * 1024, write_buf: 64 * 1024, flush_interval: Duration::from_millis(50), backoff_unit: Duration::from_millis(10) };
        let mgr = DownloadManager::build(Arc::new(move || settings.clone()), rx, dir.path().join("downloads.json"), Some(url_fn), tuning);
        tokio::spawn(mgr.clone().run());
        Env { mgr, srv, tx, dir, body_sha }
    }

    async fn wait_for(mgr: &DownloadManager, id: &str, what: &str, mut pred: impl FnMut(&TaskView) -> bool) -> TaskView {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let v = mgr.get(id).unwrap();
            if pred(&v) {
                return v;
            }
            assert!(Instant::now() < deadline, "timeout waiting for {what}: {v:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn file(path: &str, size: usize, sha: Option<String>) -> NewFile {
        NewFile { path: path.into(), size: Some(size as u64), sha256: sha }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_resume_end_to_end() {
        const SIZE: usize = 5 * 1024 * 1024;
        let e = env(SIZE, Duration::from_millis(8), 4, 1).await;
        let v = e.mgr.add_hf("test/repo", vec![file("sub/model.gguf", SIZE, Some(e.body_sha.clone()))], None).unwrap();
        let id = v.id;

        let mid = wait_for(&e.mgr, &id, "partial progress", |v| v.downloaded > 256 * 1024).await;
        assert_eq!(mid.status, Status::Downloading);
        assert_eq!(mid.files[0].connections, 4);
        assert!(mid.downloaded < SIZE as u64);

        assert!(e.mgr.pause(&id));
        let paused = e.mgr.get(&id).unwrap();
        assert_eq!(paused.status, Status::Paused);
        let frozen = paused.downloaded;
        assert!(frozen > 0 && frozen < SIZE as u64);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(e.mgr.get(&id).unwrap().downloaded, frozen, "pause must stop writes immediately");

        let models = e.dir.path().join("models/test/repo/sub");
        assert!(models.join("model.gguf.part").exists());
        assert!(!models.join("model.gguf").exists());
        // The persisted state carries the per-segment progress.
        let saved: Vec<Task> = read_json_file(&e.dir.path().join("downloads.json")).unwrap();
        assert_eq!(saved[0].status, Status::Paused);
        assert_eq!(saved[0].files[0].downloaded(), frozen);

        assert!(e.mgr.resume(&id));
        let done = wait_for(&e.mgr, &id, "completion", |v| v.status == Status::Completed || v.status == Status::Failed).await;
        assert_eq!(done.status, Status::Completed, "{done:?}");
        assert_eq!(done.downloaded, SIZE as u64);
        assert!(done.files[0].complete);
        assert!(done.finished_at.is_some());

        let final_path = models.join("model.gguf");
        assert!(final_path.exists() && !models.join("model.gguf.part").exists());
        let bytes = std::fs::read(&final_path).unwrap();
        assert_eq!(bytes, e.srv.body);
        assert_eq!(hex_sha256(&bytes), e.body_sha);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sha_mismatch_fails_and_resets() {
        const SIZE: usize = 2 * 1024 * 1024;
        let e = env(SIZE, Duration::ZERO, 2, 1).await;
        let v = e.mgr.add_hf("test/repo", vec![file("m.gguf", SIZE, Some("0".repeat(64)))], None).unwrap();
        let done = wait_for(&e.mgr, &v.id, "failure", |v| v.status == Status::Failed || v.status == Status::Completed).await;
        assert_eq!(done.status, Status::Failed);
        let err = done.error.unwrap();
        assert!(err.contains("sha256 不一致") && err.contains("最初から取り直します"), "{err}");
        let dir = e.dir.path().join("models/test/repo");
        assert!(!dir.join("m.gguf.part").exists() && !dir.join("m.gguf").exists());
        assert_eq!(done.files[0].connections, 0);
        assert_eq!(done.files[0].downloaded, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retries_transient_errors_and_unknown_size() {
        let e = env(3 * 1024 * 1024, Duration::ZERO, 1, 1).await;
        e.srv.fail_first.store(2, Ordering::SeqCst);
        // size 0 => discovered via a Range probe.
        let v = e.mgr.add_hf("test/repo", vec![file("x.bin", 0, None)], None).unwrap();
        let done = wait_for(&e.mgr, &v.id, "completion", |v| matches!(v.status, Status::Failed | Status::Completed)).await;
        assert_eq!(done.status, Status::Completed, "{done:?}");
        assert_eq!(done.total, 3 * 1024 * 1024);
        assert!(e.srv.hits.load(Ordering::SeqCst) >= 3);
        assert_eq!(std::fs::read(e.dir.path().join("models/test/repo/x.bin")).unwrap(), e.srv.body);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_deletes_part_and_resume_restarts() {
        const SIZE: usize = 4 * 1024 * 1024;
        let e = env(SIZE, Duration::from_millis(8), 2, 1).await;
        let id = e.mgr.add_hf("test/repo", vec![file("c.bin", SIZE, None)], None).unwrap().id;
        wait_for(&e.mgr, &id, "progress", |v| v.downloaded > 128 * 1024).await;
        assert!(e.mgr.cancel(&id));
        let v = e.mgr.get(&id).unwrap();
        assert_eq!(v.status, Status::Cancelled);
        assert_eq!((v.downloaded, v.files[0].connections), (0, 0));
        assert!(!e.dir.path().join("models/test/repo/c.bin.part").exists());
        assert!(!e.mgr.cancel("nope") && !e.mgr.pause("nope") && !e.mgr.resume("nope"));

        e.mgr.resume(&id);
        let done = wait_for(&e.mgr, &id, "completion", |v| matches!(v.status, Status::Failed | Status::Completed)).await;
        assert_eq!(done.status, Status::Completed, "{done:?}");
        e.mgr.remove(&id);
        assert!(e.mgr.get(&id).is_none());
        assert!(e.dir.path().join("models/test/repo/c.bin").exists(), "removing a finished task keeps the file");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrency_limit_applies_live() {
        const SIZE: usize = 3 * 1024 * 1024;
        let e = env(SIZE, Duration::from_millis(10), 1, 1).await;
        let a = e.mgr.add_hf("test/a", vec![file("a.bin", SIZE, None)], None).unwrap().id;
        tokio::time::sleep(Duration::from_millis(5)).await;
        let b = e.mgr.add_hf("test/b", vec![file("b.bin", SIZE, None)], None).unwrap().id;
        wait_for(&e.mgr, &a, "a running", |v| v.status == Status::Downloading).await;
        assert_eq!(e.mgr.get(&b).unwrap().status, Status::Queued, "limit 1: second task waits");

        // Raise the limit live: b starts without waiting for a.
        e.tx.send_replace(profile(1, 2, None));
        wait_for(&e.mgr, &b, "b running", |v| v.status == Status::Downloading).await;
        assert_eq!(e.mgr.get(&a).unwrap().status, Status::Downloading);

        // Lower it again: the newest task goes back to the queue, keeping its progress.
        e.tx.send_replace(profile(1, 1, None));
        let bv = wait_for(&e.mgr, &b, "b demoted", |v| v.status == Status::Queued).await;
        assert!(bv.files[0].connections > 0);
        assert_eq!(e.mgr.get(&a).unwrap().status, Status::Downloading);

        // Both finish eventually, a first.
        for id in [&a, &b] {
            let done = wait_for(&e.mgr, id, "completion", |v| matches!(v.status, Status::Failed | Status::Completed)).await;
            assert_eq!(done.status, Status::Completed, "{done:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bandwidth_limit_slows_transfer() {
        const SIZE: usize = 2 * 1024 * 1024;
        let e = env(SIZE, Duration::ZERO, 2, 1).await;
        e.tx.send_replace(profile(2, 1, Some(2 * 1024 * 1024))); // 2 MiB/s, burst 512 KiB
        let start = Instant::now();
        let id = e.mgr.add_hf("test/repo", vec![file("slow.bin", SIZE, None)], None).unwrap().id;
        let done = wait_for(&e.mgr, &id, "completion", |v| matches!(v.status, Status::Failed | Status::Completed)).await;
        assert_eq!(done.status, Status::Completed, "{done:?}");
        assert!(start.elapsed() > Duration::from_millis(600), "2 MiB at 2 MiB/s with a 0.5 MiB burst must take > 0.6 s, took {:?}", start.elapsed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_bad_input_and_duplicates() {
        let e = env(1024, Duration::from_secs(5), 1, 1).await;
        assert!(e.mgr.add_hf("test/repo", vec![], None).is_err());
        assert!(e.mgr.add_hf("../evil", vec![file("a", 1, None)], None).is_err());
        assert!(e.mgr.add_hf("test/repo", vec![file("../../etc/passwd", 1, None)], None).is_err());
        assert!(e.mgr.add_ollama("  ").is_err());
        let first = e.mgr.add_hf("test/repo", vec![file("a.bin", 1024, None)], None).unwrap();
        assert!(e.mgr.add_hf("test/repo", vec![file("a.bin", 1024, None)], None).is_err(), "same file twice");
        assert!(e.mgr.add_hf("test/repo", vec![file("b.bin", 1024, None)], None).is_ok());
        // Newest first.
        assert_eq!(e.mgr.list().len(), 2);
        assert_ne!(e.mgr.list()[0].id, first.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_requeues_running_tasks() {
        let e = env(1024, Duration::from_secs(5), 1, 1).await;
        let id = e.mgr.add_hf("test/repo", vec![file("a.bin", 1024, None)], None).unwrap().id;
        wait_for(&e.mgr, &id, "running", |v| v.status == Status::Downloading).await;
        e.mgr.persist();
        let (_tx, rx) = watch::channel(profile(1, 1, None));
        let reloaded = DownloadManager::build(Arc::new(Settings::default), rx, e.dir.path().join("downloads.json"), None, Tuning::default());
        assert_eq!(reloaded.get(&id).unwrap().status, Status::Queued);
    }
}
