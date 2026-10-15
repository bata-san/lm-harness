//! Strata lifecycle: idle unload, watchdog restarts, swap-on-demand, native passthrough.
//!
//! Strata (Niko1221/Strata) decides VRAM / RAM / SSD expert placement itself, and nothing here
//! tiers memory. The harness only decides *when a server runs and whether it holds its model*:
//! * `--lazy` (launch default) starts the API without loading the engine;
//! * [`Supervisor::sweep`] unloads a model after it has been idle (`POST /v1/unload`). An active
//!   request blocks an unload for as long as its response body is alive ([`ActivityGuard`]);
//! * a Strata started by the harness that dies unexpectedly is relaunched with exponential
//!   backoff, and a manual stop is never undone;
//! * a request that names a Strata config which is not running swaps the running one out.
//!
//! Routes: `/api/strata/*` (session only, see [`router`]).
//!
//! Auth towards Strata: its control endpoints and API take `Authorization: Bearer <api_key>`
//! when the run config (`strata-<model>.json`) sets an `api_key`. The key is read from that file
//! for each call and is never logged.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin::{audit, session_only, Admin};
use crate::config::Settings;
use crate::error::{ApiError, ApiResult};
use crate::instances::{Instance, LaunchSpec};
use crate::mode::Mode;
use crate::procs::Status;
use crate::proxy::{self, Target};
use crate::runtimes;
use crate::state::AppState;

/// How often the lifecycle loop looks at the servers (it also runs at once on a mode switch).
const TICK: Duration = Duration::from_secs(2);
/// A swap waits this long for a busy Strata to finish before giving up (503).
const IDLE_WAIT: Duration = Duration::from_secs(60);
/// A newly launched Strata must answer health checks within this time.
const READY_WAIT: Duration = Duration::from_secs(120);
/// `/v1/load` can take minutes for a large model.
const LOAD_TIMEOUT: Duration = Duration::from_secs(600);
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
/// After a refused unload, the loop waits this long before trying the instance again.
const UNLOAD_RETRY: Duration = Duration::from_secs(60);

const RESTART_BASE_S: u64 = 5;
const RESTART_CAP: Duration = Duration::from_secs(300);
const RESTART_WINDOW: Duration = Duration::from_secs(600);
/// This many unexpected exits inside [`RESTART_WINDOW`] (with no healthy start in between) stop the watchdog.
const RESTART_MAX_FAILURES: usize = 5;

/// Loopback client: never through an environment proxy, never follows redirects.
static CLIENT: LazyLock<reqwest::Client> =
    LazyLock::new(|| reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(5)).build().expect("build strata client"));

// ═════════════════════════════════════════════════════════════════ activity
/// What the idle decision needs to know about one server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Requests whose response is still being produced (streams included).
    pub active: usize,
    /// Last time a request started or ended (or the server was launched / explicitly loaded).
    pub last: Instant,
    /// The engine was unloaded and no request has arrived since.
    pub unloaded: bool,
}

struct ActivityState {
    active: usize,
    last: Instant,
    unloaded: bool,
}

/// Per-server request accounting. Shared between the proxy and the lifecycle loop.
pub struct Activity {
    inner: Mutex<ActivityState>,
}

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(ActivityState { active: 0, last: Instant::now(), unloaded: false }) })
    }

    /// Count one request until the returned guard is dropped.
    pub fn begin(self: &Arc<Self>) -> ActivityGuard {
        {
            let mut st = self.inner.lock();
            st.active += 1;
            st.last = Instant::now();
            st.unloaded = false;
        }
        ActivityGuard { act: self.clone() }
    }

    pub fn snapshot(&self) -> Snapshot {
        let st = self.inner.lock();
        Snapshot { active: st.active, last: st.last, unloaded: st.unloaded }
    }

    pub fn mark_unloaded(&self) {
        self.inner.lock().unloaded = true;
    }

    /// An explicit load: the idle clock starts now.
    pub fn touch(&self) {
        let mut st = self.inner.lock();
        st.last = Instant::now();
        st.unloaded = false;
    }
}

/// Holds one in-flight request open. Created by [`Activity::begin`]; dropping it ends the request.
pub struct ActivityGuard {
    act: Arc<Activity>,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let mut st = self.act.inner.lock();
        st.active = st.active.saturating_sub(1);
        st.last = Instant::now();
    }
}

/// Keep `guard` alive for as long as the stream is alive: an in-flight SSE stream (or a body the
/// client has not finished reading) counts as activity until it ends or is dropped.
pub fn guard_stream<S>(stream: S, guard: Option<ActivityGuard>) -> impl Stream<Item = S::Item> + Send + 'static
where
    S: Stream + Send + 'static,
    S::Item: Send + 'static,
{
    futures::stream::unfold((Box::pin(stream), guard), |(mut s, g)| async move {
        let item = s.next().await?;
        Some((item, (s, g)))
    })
}

/// Guard for a request sent to `target` (None when the instance is already gone).
pub fn begin_activity(state: &AppState, target: &Target) -> Option<ActivityGuard> {
    state.instances.get(&target.id).map(|i| i.activity.begin())
}

// ═════════════════════════════════════════════════════════════════ idle decision
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Keep,
    Unload,
}

/// Idle limits in seconds; 0 means never unload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdlePolicy {
    pub full_s: u64,
    pub shared_s: u64,
}

impl IdlePolicy {
    pub fn from_settings(s: &Settings) -> Self {
        Self { full_s: s.strata_idle_unload_s, shared_s: s.strata_shared_idle_unload_s }
    }

    /// The limit that applies in `mode` (shared mode uses the shorter one).
    pub fn limit_s(&self, mode: Mode) -> u64 {
        match mode {
            Mode::Full => self.full_s,
            Mode::Shared => self.shared_s,
        }
    }
}

/// Whether to unload one server now. Active requests and already-unloaded engines always keep.
pub fn decide(policy: IdlePolicy, mode: Mode, now: Instant, snap: Snapshot) -> Decision {
    let limit = policy.limit_s(mode);
    if snap.active > 0 || snap.unloaded || limit == 0 {
        return Decision::Keep;
    }
    if now.saturating_duration_since(snap.last) >= Duration::from_secs(limit) {
        Decision::Unload
    } else {
        Decision::Keep
    }
}

// ═════════════════════════════════════════════════════════════════ watchdog
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    RetryIn(Duration),
    GiveUp,
}

/// Delay before relaunch after the `n`-th consecutive failure (1-based): 5 s, 10 s, 20 s, ... capped.
pub fn backoff(n: usize) -> Duration {
    let shift = (n.max(1) - 1).min(16) as u32;
    Duration::from_secs(RESTART_BASE_S << shift).min(RESTART_CAP)
}

/// Unexpected exits of one supervised server. A healthy start clears them.
#[derive(Debug, Default)]
pub struct Restarts {
    failures: Vec<Instant>,
}

impl Restarts {
    pub fn record_failure(&mut self, now: Instant) -> Verdict {
        self.failures.retain(|t| now.saturating_duration_since(*t) < RESTART_WINDOW);
        self.failures.push(now);
        if self.failures.len() >= RESTART_MAX_FAILURES {
            Verdict::GiveUp
        } else {
            Verdict::RetryIn(backoff(self.failures.len()))
        }
    }

    pub fn record_healthy(&mut self) {
        self.failures.clear();
    }

    pub fn recent(&self, now: Instant) -> usize {
        self.failures.iter().filter(|t| now.saturating_duration_since(**t) < RESTART_WINDOW).count()
    }
}

/// A Strata server the watchdog keeps running.
struct Supervised {
    key: u64,
    /// Launch spec with the port and host pinned, so a relaunch lands on the same address.
    spec: LaunchSpec,
    /// Id of the server that should be running; `None` while waiting for the relaunch.
    instance: Option<String>,
    restarts: Restarts,
    retry_at: Option<Instant>,
    given_up: bool,
}

/// An exit that the watchdog must act on. A manual stop (or a swap) ends in `Stopped`, which is not one.
pub fn is_unexpected_exit(status: Status) -> bool {
    matches!(status, Status::Failed | Status::Exited)
}

#[derive(Default)]
struct Table {
    entries: Vec<Supervised>,
    /// Instance ids already looked at, so each server is adopted once.
    seen: HashSet<String>,
}

/// Lifecycle state for all Strata servers: watchdog, idle unloads, swaps.
#[derive(Default)]
pub struct Supervisor {
    table: Mutex<Table>,
    /// instance id -> earliest time to try an unload again (after a refusal or failure).
    unload_retry: Mutex<HashMap<String, Instant>>,
    /// Last watchdog / swap problem, shown in the status route.
    last_error: Mutex<Option<String>>,
    next_key: AtomicU64,
    /// Serialises swaps, explicit starts and watchdog relaunches (they all change which Strata runs).
    swap: tokio::sync::Mutex<()>,
    stopping: AtomicBool,
}

impl Supervisor {
    /// Ask the lifecycle loop to exit at its next tick (called on shutdown).
    pub fn request_stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }

    pub fn watchdog_failures(&self, now: Instant) -> usize {
        self.table.lock().entries.iter().map(|e| e.restarts.recent(now)).sum()
    }

    /// One pass of the lifecycle loop: watchdog, then idle unloads.
    pub async fn sweep(&self, state: &AppState, now: Instant) {
        let settings = state.settings.get();
        let strata: Vec<Arc<Instance>> = state.instances.all().into_iter().filter(|i| i.runtime == "strata").collect();
        if settings.strata_watchdog {
            self.adopt(&strata);
            self.table.lock().entries.retain_mut(|e| self.settle(e, &strata, now));
            self.relaunch_due(state, now).await;
        } else {
            self.table.lock().entries.clear();
        }
        self.unload_idle(&settings, state.profile().mode, &strata, now).await;
    }

    /// Start supervising servers this run has not seen yet (manual starts, swaps, autostart).
    fn adopt(&self, strata: &[Arc<Instance>]) {
        let mut table = self.table.lock();
        let Table { entries, seen } = &mut *table;
        for inst in strata {
            if !seen.insert(inst.id().to_string()) || inst.proc.status() == Status::Stopped {
                continue;
            }
            let mut spec = inst.spec.clone();
            spec.port = Some(inst.port);
            spec.host = Some(inst.host.clone());
            entries.push(Supervised {
                key: self.next_key.fetch_add(1, Ordering::Relaxed),
                spec,
                instance: Some(inst.id().to_string()),
                restarts: Restarts::default(),
                retry_at: None,
                given_up: false,
            });
        }
    }

    /// Update one entry from its server's state. Returns `false` when the entry is no longer needed.
    fn settle(&self, e: &mut Supervised, strata: &[Arc<Instance>], now: Instant) -> bool {
        let Some(id) = e.instance.clone() else { return true };
        let Some(inst) = strata.iter().find(|i| i.id() == id) else {
            // forgotten by the manager (removed after it died)
            return false;
        };
        if inst.alive() {
            if inst.ready() {
                e.restarts.record_healthy();
            }
            return true;
        }
        // a manual stop (or a swap) ends in `Stopped`: that entry is done, never relaunched
        if !is_unexpected_exit(inst.proc.status()) {
            return false;
        }
        let code = inst.proc.returncode().map_or_else(|| "?".to_string(), |c| c.to_string());
        let msg = format!("{} が予期せず終了しました (終了コード {code})", e.spec.model);
        self.fail(e, now, msg);
        true
    }

    /// Record a failure of `e` and schedule its relaunch, or give up.
    fn fail(&self, e: &mut Supervised, now: Instant, msg: String) {
        e.instance = None;
        match e.restarts.record_failure(now) {
            Verdict::RetryIn(d) => {
                tracing::warn!("strata watchdog: {msg}; relaunch in {}s", d.as_secs());
                e.retry_at = Some(now + d);
                *self.last_error.lock() = Some(msg);
            }
            Verdict::GiveUp => {
                tracing::error!("strata watchdog: giving up on {}", e.spec.model);
                e.retry_at = None;
                e.given_up = true;
                *self.last_error.lock() = Some(format!("{msg}。連続失敗のため自動再起動を停止しました (手動で起動してください)"));
            }
        }
    }

    async fn relaunch_due(&self, state: &AppState, now: Instant) {
        let due: Vec<(u64, LaunchSpec)> =
            self.table.lock().entries.iter().filter(|e| e.instance.is_none() && !e.given_up && e.retry_at.is_some_and(|t| t <= now)).map(|e| (e.key, e.spec.clone())).collect();
        if due.is_empty() {
            return;
        }
        let _swap = self.swap.lock().await;
        for (key, spec) in due {
            if self.stopping() {
                return;
            }
            let model = spec.model.clone();
            let launched = state.instances.launch(state, spec).await;
            let mut table = self.table.lock();
            let Table { entries, seen } = &mut *table;
            let Some(e) = entries.iter_mut().find(|e| e.key == key) else { continue };
            match launched {
                Ok(inst) => {
                    tracing::info!("strata watchdog: relaunched {model}");
                    let id = inst.id().to_string();
                    seen.insert(id.clone());
                    e.instance = Some(id);
                    e.retry_at = None;
                }
                Err(err) => self.fail(e, now, format!("{model} の再起動に失敗しました: {err:#}")),
            }
        }
    }

    /// Unload models that have been idle for longer than the limit of the current mode.
    async fn unload_idle(&self, settings: &Settings, mode: Mode, strata: &[Arc<Instance>], now: Instant) {
        let policy = IdlePolicy::from_settings(settings);
        for inst in strata.iter().filter(|i| i.alive() && i.ready()) {
            let id = inst.id().to_string();
            if self.unload_retry.lock().get(&id).is_some_and(|t| *t > now) {
                continue;
            }
            if decide(policy, mode, now, inst.activity.snapshot()) == Decision::Keep {
                continue;
            }
            match control_instance(inst, settings, "unload", UNLOAD_TIMEOUT).await {
                Ok(Control::Done) => {
                    inst.activity.mark_unloaded();
                    tracing::info!("strata: unloaded {} after idle", inst.model);
                }
                // Strata has a request queued or running: try again on the next tick.
                Ok(Control::Busy) => {}
                Ok(Control::Rejected(code)) => {
                    self.unload_retry.lock().insert(id, now + UNLOAD_RETRY);
                    tracing::warn!("strata: unload of {} refused (HTTP {code})", inst.model);
                }
                Err(e) => {
                    self.unload_retry.lock().insert(id, now + UNLOAD_RETRY);
                    tracing::warn!("strata: unload of {} failed ({e})", inst.model);
                }
            }
        }
        self.unload_retry.lock().retain(|_, t| *t > now);
    }

    fn forget_given_up(&self, config: &str) {
        self.table.lock().entries.retain(|e| !(e.given_up && e.spec.model == config));
    }

    /// Make the Strata config a request names the running one. Returns once it answers health checks.
    pub async fn ensure_for_request(&self, state: &AppState, requested: &str) -> Result<(), SwapError> {
        let settings = state.settings.get();
        if !settings.strata_swap_on_demand {
            return Ok(());
        }
        let configs = runtimes::strata_configs(&settings);
        let others: Vec<String> = proxy::targets(state).iter().filter(|t| t.runtime != "strata").map(|t| norm(&t.model)).collect();
        let Some(config) = match_config(requested, &configs, &others) else { return Ok(()) };
        if let Some(inst) = running_for(state, config) {
            return wait_ready(&inst).await;
        }
        let _swap = self.swap.lock().await;
        let inst = match running_for(state, config) {
            Some(inst) => inst,
            None => self.switch_to(state, config, None).await?,
        };
        wait_ready(&inst).await
    }

    /// Explicit start (`POST /api/strata/start`): the config runs after this returns, not necessarily ready.
    pub async fn start(&self, state: &AppState, config: &str, port: Option<u16>) -> Result<Arc<Instance>, SwapError> {
        let _swap = self.swap.lock().await;
        if let Some(inst) = running_for(state, config) {
            return Ok(inst);
        }
        self.forget_given_up(config);
        self.switch_to(state, config, port).await
    }

    /// Stop every other Strata (each must be idle) and launch `config`. Caller holds the swap lock.
    async fn switch_to(&self, state: &AppState, config: &str, port: Option<u16>) -> Result<Arc<Instance>, SwapError> {
        for other in alive_strata(state).into_iter().filter(|i| i.model != config) {
            if !wait_idle(&other, IDLE_WAIT).await {
                return Err(SwapError::Busy(format!("Strata ({}) が処理中のため切り替えられません。しばらくして再試行してください", other.model)));
            }
            tracing::info!("strata: stopping {} to run {config}", other.model);
            other.proc.stop().await;
        }
        let spec = LaunchSpec { runtime: "strata".into(), model: config.into(), port, ..LaunchSpec::default() };
        state.instances.launch(state, spec).await.map_err(|e| SwapError::Failed(format!("{config} の起動に失敗しました: {e:#}")))
    }
}

/// Lifecycle loop, spawned from `main`. Ends after [`Supervisor::request_stop`].
pub async fn run(state: AppState) {
    let mut ticker = tokio::time::interval(TICK);
    let mut mode_rx = state.mode_rx.clone();
    let mut mode_open = true;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            // a mode switch re-evaluates at once, so the shorter shared limit applies immediately
            changed = mode_rx.changed(), if mode_open => {
                if changed.is_err() {
                    mode_open = false;
                }
            }
        }
        if state.strata.stopping() {
            break;
        }
        state.strata.sweep(&state, Instant::now()).await;
    }
}

// ═════════════════════════════════════════════════════════════════ swap helpers
#[derive(Debug)]
pub enum SwapError {
    /// The running Strata is busy and did not finish within the wait.
    Busy(String),
    Failed(String),
}

impl SwapError {
    pub fn message(&self) -> &str {
        match self {
            SwapError::Busy(m) | SwapError::Failed(m) => m,
        }
    }
}

fn swap_api_error(e: SwapError) -> ApiError {
    match e {
        SwapError::Busy(m) => ApiError(StatusCode::CONFLICT, m),
        SwapError::Failed(m) => ApiError(StatusCode::SERVICE_UNAVAILABLE, m),
    }
}

/// Lower-cased file name without `.json` / `.gguf` (`org/strata-a.json` -> `strata-a`).
fn norm(name: &str) -> String {
    let base = name.trim().rsplit(['/', '\\']).next().unwrap_or("").to_ascii_lowercase();
    base.strip_suffix(".json").or_else(|| base.strip_suffix(".gguf")).unwrap_or(base.as_str()).to_string()
}

/// The Strata config a requested model name means. `strata-foo` / `strata-foo.json` match the
/// config `strata-foo.json`; the short name `foo` matches it too, unless a running non-Strata
/// server already answers to `foo`. `None` when the name is not a Strata config.
pub fn match_config<'a>(requested: &str, configs: &'a [String], other_models: &[String]) -> Option<&'a str> {
    let req = norm(requested);
    if req.is_empty() {
        return None;
    }
    if let Some(c) = configs.iter().find(|c| norm(c) == req) {
        return Some(c.as_str());
    }
    if other_models.contains(&req) {
        return None;
    }
    configs.iter().find(|c| norm(c).strip_prefix("strata-") == Some(req.as_str())).map(String::as_str)
}

fn alive_strata(state: &AppState) -> Vec<Arc<Instance>> {
    state.instances.alive_instances().into_iter().filter(|i| i.runtime == "strata").collect()
}

fn running_for(state: &AppState, config: &str) -> Option<Arc<Instance>> {
    alive_strata(state).into_iter().find(|i| i.model == config)
}

fn newest_strata(state: &AppState) -> Option<Arc<Instance>> {
    alive_strata(state).into_iter().max_by(|a, b| a.proc.started_at.total_cmp(&b.proc.started_at))
}

async fn wait_idle(inst: &Instance, max: Duration) -> bool {
    let deadline = Instant::now() + max;
    loop {
        if inst.activity.snapshot().active == 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_ready(inst: &Instance) -> Result<(), SwapError> {
    let deadline = Instant::now() + READY_WAIT;
    while !inst.ready() {
        if !inst.alive() {
            return Err(SwapError::Failed(format!("{} が起動直後に終了しました。ログを確認してください", inst.model)));
        }
        if Instant::now() >= deadline {
            return Err(SwapError::Failed(format!("{} の起動待ちがタイムアウトしました", inst.model)));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}

// ═════════════════════════════════════════════════════════════════ Strata control API
/// Result of `POST /v1/load` or `/v1/unload`.
#[derive(Debug, PartialEq, Eq)]
pub enum Control {
    Done,
    /// 409: a request is active or queued inside Strata.
    Busy,
    Rejected(u16),
}

/// `POST {base}/v1/{action}` with body `{}`. Strata answers `{}` by acting on its single configured
/// model. Sends no `Origin` header. Errors name only the failure class, never the URL or key.
pub async fn control(base: &str, action: &str, key: Option<&str>, timeout: Duration) -> Result<Control, String> {
    let mut req = CLIENT.post(format!("{base}/v1/{action}")).timeout(timeout).json(&json!({}));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await.map_err(|e| {
        if e.is_timeout() {
            "timeout"
        } else if e.is_connect() {
            "connect"
        } else {
            "request"
        }
        .to_string()
    })?;
    let code = resp.status();
    Ok(if code.is_success() {
        Control::Done
    } else if code == StatusCode::CONFLICT {
        Control::Busy
    } else {
        Control::Rejected(code.as_u16())
    })
}

async fn control_instance(inst: &Instance, settings: &Settings, action: &str, timeout: Duration) -> Result<Control, String> {
    let base = proxy::upstream_base(&inst.host, inst.port);
    let key = api_key_for(settings, &inst.model);
    control(&base, action, key.as_deref(), timeout).await
}

/// `api_key` of a Strata run config in the Strata checkout, if it sets one.
pub fn api_key_for(settings: &Settings, config: &str) -> Option<String> {
    if !runtimes::valid_strata_config(config) {
        return None;
    }
    let text = std::fs::read_to_string(settings.strata_dir().join(config)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("api_key")?.as_str().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned)
}

/// Client headers minus every credential, plus Strata's own key when its config sets one.
pub fn upstream_headers(src: &HeaderMap, key: Option<&str>) -> HeaderMap {
    let mut h = proxy::forward_headers(src, false);
    if let Some(k) = key {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {k}")) {
            h.insert(header::AUTHORIZATION, v);
        }
    }
    h
}

/// Relay a request to a Strata server unchanged (Strata speaks Anthropic and OpenAI Responses
/// itself). Only the credentials change. Errors are `(status, machine code, message)`.
pub async fn passthrough(state: &AppState, target: &Target, path: &str, headers: &HeaderMap, body: Bytes) -> Result<Response, (StatusCode, &'static str, String)> {
    let key = api_key_for(&state.settings.get(), &target.model);
    let guard = begin_activity(state, target);
    proxy::relay(target, Method::POST, path, upstream_headers(headers, key.as_deref()), Some(reqwest::Body::from(body)), guard).await
}

/// `loaded` and `auto_load` from Strata's `/v1/status`.
pub fn parse_engine_status(v: &Value) -> (Option<bool>, Option<bool>) {
    (v.get("loaded").and_then(Value::as_bool), v.get("auto_load").and_then(Value::as_bool))
}

async fn engine_status(base: &str) -> (Option<bool>, Option<bool>) {
    let fetched = async {
        let resp = CLIENT.get(format!("{base}/v1/status")).timeout(STATUS_TIMEOUT).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    };
    match fetched.await {
        Some(v) => parse_engine_status(&v),
        None => (None, None),
    }
}

// ═════════════════════════════════════════════════════════════════ launch args
fn flag_of(arg: &str) -> &str {
    arg.split_once('=').map_or(arg, |(f, _)| f)
}

/// Arguments for a Strata launch: `tuned` first, then the user's args (which win on conflicts).
/// `--lazy` is added when `lazy_load` is on, unless the user already chose `--lazy` or `--no-lazy`.
pub fn launch_args(lazy_load: bool, tuned: &[String], user: &[String]) -> Vec<String> {
    let user_chose_lazy = user.iter().any(|a| matches!(flag_of(a), "--lazy" | "--no-lazy"));
    let mut base: Vec<String> = tuned.to_vec();
    if lazy_load && !user_chose_lazy {
        base.insert(0, "--lazy".into());
    }
    if base.is_empty() {
        return user.to_vec();
    }
    crate::tuning::merge_args(&base, user)
}

// ═════════════════════════════════════════════════════════════════ routes
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/strata/status", get(status_route))
        .route("/api/strata/start", post(start_route))
        .route("/api/strata/load", post(load_route))
        .route("/api/strata/unload", post(unload_route))
        .route_layer(middleware::from_fn(session_only))
}

async fn status_route(_a: Admin, State(st): State<AppState>) -> Json<Value> {
    let settings = st.settings.get();
    let now = Instant::now();
    let newest = newest_strata(&st);
    let (loaded, auto_load, idle_for_s, idle_limit_s) = match &newest {
        Some(inst) => {
            let snap = inst.activity.snapshot();
            let idle = if snap.active > 0 { 0.0 } else { now.saturating_duration_since(snap.last).as_secs_f64() };
            let (loaded, auto_load) = engine_status(&proxy::upstream_base(&inst.host, inst.port)).await;
            (loaded, auto_load, Some(idle), Some(IdlePolicy::from_settings(&settings).limit_s(st.profile().mode)))
        }
        None => (None, None, None, None),
    };
    Json(json!({
        "running": newest.is_some(),
        "instance": newest.as_ref().map(|i| json!({"id": i.id(), "name": i.proc.name, "config": i.model, "port": i.port, "ready": i.ready()})),
        "loaded": loaded,
        "auto_load": auto_load,
        "idle_for_s": idle_for_s,
        "idle_limit_s": idle_limit_s,
        "configs": runtimes::strata_configs(&settings),
        "last_error": st.strata.last_error(),
        "watchdog": { "enabled": settings.strata_watchdog, "failures": st.strata.watchdog_failures(now) },
    }))
}

#[derive(Deserialize)]
struct StartBody {
    config: String,
    #[serde(default)]
    port: Option<u16>,
}

async fn start_route(a: Admin, State(st): State<AppState>, Json(body): Json<StartBody>) -> ApiResult<Value> {
    if !runtimes::strata_configs(&st.settings.get()).contains(&body.config) {
        return Err(ApiError::bad_request("その設定ファイルはありません (strata-<model>.json)"));
    }
    let inst = st.strata.start(&st, &body.config, body.port.filter(|p| *p != 0)).await.map_err(swap_api_error)?;
    audit(&st, &a, "strata.start", &format!("config={}", body.config));
    Ok(Json(inst.to_json(st.profile().mode)))
}

fn outcome_label(outcome: &Result<Control, String>) -> &'static str {
    match outcome {
        Ok(Control::Done) => "ok",
        Ok(Control::Busy) => "busy",
        _ => "failed",
    }
}

fn control_result(outcome: Result<Control, String>) -> ApiResult<Value> {
    match outcome {
        Ok(Control::Done) => Ok(Json(json!({"ok": true}))),
        Ok(Control::Busy) => Err(ApiError(StatusCode::CONFLICT, "Strata はリクエストを処理中または待機中のため操作できません".into())),
        Ok(Control::Rejected(404)) => Err(ApiError::not_found("Strata がこの設定を認識しません")),
        Ok(Control::Rejected(code)) => Err(ApiError::bad_gateway(format!("Strata が HTTP {code} を返しました"))),
        Err(e) => Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, format!("Strata に接続できません ({e})"))),
    }
}

async fn load_route(a: Admin, State(st): State<AppState>) -> ApiResult<Value> {
    let inst = newest_strata(&st).ok_or_else(|| ApiError::not_found("Strata は起動していません"))?;
    let outcome = control_instance(&inst, &st.settings.get(), "load", LOAD_TIMEOUT).await;
    if matches!(outcome, Ok(Control::Done)) {
        inst.activity.touch();
    }
    audit(&st, &a, "strata.load", &format!("config={} {}", inst.model, outcome_label(&outcome)));
    control_result(outcome)
}

async fn unload_route(a: Admin, State(st): State<AppState>) -> ApiResult<Value> {
    let inst = newest_strata(&st).ok_or_else(|| ApiError::not_found("Strata は起動していません"))?;
    let outcome = control_instance(&inst, &st.settings.get(), "unload", UNLOAD_TIMEOUT).await;
    if matches!(outcome, Ok(Control::Done)) {
        inst.activity.mark_unloaded();
    }
    audit(&st, &a, "strata.unload", &format!("config={} {}", inst.model, outcome_label(&outcome)));
    control_result(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::routing::post as axum_post;
    use futures::stream;

    const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn strata_target(base: String) -> Target {
        Target { id: "t1".into(), model: "strata-a.json".into(), name: "strata-a.json".into(), runtime: "strata".into(), ready: true, base, served: "strata-a.json".into(), started_at: 0.0 }
    }

    /// Fake Strata: `/v1/messages` streams SSE and echoes the credentials it received.
    fn fake_messages() -> Router {
        Router::new().route(
            "/v1/messages",
            axum_post(|headers: HeaderMap| async move {
                let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("none").to_string();
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .header("x-seen-auth", auth)
                    .header("x-seen-xapi", headers.contains_key("x-api-key").to_string())
                    .body(axum::body::Body::from(SSE))
                    .unwrap()
            }),
        )
    }

    /// Fake Strata `/v1/unload`: refuses without the Bearer key or a JSON body, and answers 409 while `busy`.
    fn fake_unload(busy: bool) -> Router {
        Router::new().route(
            "/v1/unload",
            axum_post(move |headers: HeaderMap| async move {
                let json_body = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"));
                let keyed = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some("Bearer strata-key");
                if !json_body || !keyed || headers.contains_key(header::ORIGIN) {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                if busy {
                    (StatusCode::CONFLICT, Json(json!({"detail": "busy"}))).into_response()
                } else {
                    Json(json!({"ok": true})).into_response()
                }
            }),
        )
    }

    fn at(secs: u64) -> Instant {
        // a fixed origin is enough: only differences matter
        static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
        *ORIGIN + Duration::from_secs(secs)
    }

    #[test]
    fn idle_decision_respects_activity_mode_and_limits() {
        let p = IdlePolicy { full_s: 600, shared_s: 60 };
        let snap = |active: usize, unloaded: bool| Snapshot { active, last: at(0), unloaded };
        // 120 s idle: kept in full mode, unloaded in shared mode (the shorter limit)
        assert_eq!(decide(p, Mode::Full, at(120), snap(0, false)), Decision::Keep);
        assert_eq!(decide(p, Mode::Shared, at(120), snap(0, false)), Decision::Unload);
        // exactly at the limit counts as idle
        assert_eq!(decide(p, Mode::Full, at(600), snap(0, false)), Decision::Unload);
        // an active request is never idle, however long ago it started
        assert_eq!(decide(p, Mode::Shared, at(10_000), snap(1, false)), Decision::Keep);
        // already unloaded: nothing to do
        assert_eq!(decide(p, Mode::Shared, at(10_000), snap(0, true)), Decision::Keep);
        // 0 means never
        let never = IdlePolicy { full_s: 0, shared_s: 0 };
        assert_eq!(decide(never, Mode::Shared, at(10_000), snap(0, false)), Decision::Keep);
        assert_eq!(p.limit_s(Mode::Shared), 60);
    }

    #[test]
    fn backoff_schedule_caps_and_gives_up() {
        assert_eq!((1..=4).map(|n| backoff(n).as_secs()).collect::<Vec<_>>(), vec![5, 10, 20, 40]);
        assert_eq!(backoff(7), Duration::from_secs(300));
        assert_eq!(backoff(50), Duration::from_secs(300));

        let mut r = Restarts::default();
        for i in 0..4 {
            assert!(matches!(r.record_failure(at(i)), Verdict::RetryIn(_)));
        }
        assert_eq!(r.record_failure(at(4)), Verdict::GiveUp, "fifth failure inside 10 min");
    }

    #[test]
    fn watchdog_window_and_healthy_reset() {
        let mut r = Restarts::default();
        for i in 0..4 {
            r.record_failure(at(i));
        }
        // the old failures fell out of the window: this is the first of a new streak
        assert_eq!(r.record_failure(at(4 + 700)), Verdict::RetryIn(Duration::from_secs(5)));
        assert_eq!(r.recent(at(4 + 700)), 1);

        let mut r = Restarts::default();
        r.record_failure(at(0));
        r.record_failure(at(1));
        r.record_healthy();
        assert_eq!(r.recent(at(2)), 0);
        assert_eq!(r.record_failure(at(2)), Verdict::RetryIn(Duration::from_secs(5)));
    }

    #[test]
    fn failing_entries_schedule_relaunch_then_give_up() {
        let sup = Supervisor::default();
        let mut e = Supervised {
            key: 1,
            spec: LaunchSpec { runtime: "strata".into(), model: "strata-a.json".into(), port: Some(8090), ..Default::default() },
            instance: Some("abc".into()),
            restarts: Restarts::default(),
            retry_at: None,
            given_up: false,
        };
        sup.fail(&mut e, at(0), "exited".into());
        assert_eq!(e.instance, None);
        assert_eq!(e.retry_at, Some(at(5)));
        assert!(!e.given_up);
        assert_eq!(sup.last_error().as_deref(), Some("exited"));
        for i in 1..5 {
            sup.fail(&mut e, at(i), format!("exit {i}"));
        }
        assert!(e.given_up && e.retry_at.is_none());
        assert!(sup.last_error().unwrap().contains("停止しました"));
    }

    #[tokio::test]
    async fn real_exits_are_classified_for_the_watchdog() {
        use crate::procs::Proc;
        use std::collections::BTreeMap;
        let crash = Proc::new("crash", vec!["sh".into(), "-c".into(), "exit 3".into()], BTreeMap::new(), None, "instance", serde_json::Map::new()).start().await;
        for _ in 0..200 {
            if !crash.alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(is_unexpected_exit(crash.status()), "{:?}", crash.status());

        let manual = Proc::new("manual", vec!["sleep".into(), "30".into()], BTreeMap::new(), None, "instance", serde_json::Map::new()).start().await;
        manual.stop_within(Duration::from_secs(5)).await;
        assert_eq!(manual.status(), Status::Stopped);
        assert!(!is_unexpected_exit(manual.status()), "a manual stop must not trigger a restart");
    }

    #[test]
    fn config_matching_by_name() {
        let configs = vec!["strata-foo.json".to_string(), "strata-bar.json".to_string()];
        let none: Vec<String> = vec![];
        assert_eq!(match_config("strata-foo", &configs, &none), Some("strata-foo.json"));
        assert_eq!(match_config("strata-foo.json", &configs, &none), Some("strata-foo.json"));
        assert_eq!(match_config("foo", &configs, &none), Some("strata-foo.json"));
        assert_eq!(match_config("Foo.json", &configs, &none), Some("strata-foo.json"));
        assert_eq!(match_config("org/strata-bar", &configs, &none), Some("strata-bar.json"));
        assert_eq!(match_config("claude-x", &configs, &none), None);
        assert_eq!(match_config("", &configs, &none), None);
        // a running non-Strata server owns the short name
        let llama = vec!["foo".to_string()];
        assert_eq!(match_config("foo", &configs, &llama), None);
        assert_eq!(match_config("strata-foo", &configs, &llama), Some("strata-foo.json"));
    }

    #[test]
    fn launch_args_add_lazy_unless_user_chose() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        assert_eq!(launch_args(true, &[], &[]), s(&["--lazy"]));
        assert_eq!(launch_args(false, &[], &s(&["--port", "9000"])), s(&["--port", "9000"]));
        assert!(!launch_args(true, &[], &s(&["--no-lazy"])).contains(&"--lazy".to_string()));
        assert_eq!(launch_args(true, &[], &s(&["--lazy"])).iter().filter(|a| *a == "--lazy").count(), 1);
        // shared-mode tuning survives, and the user's value overrides it
        let tuned = s(&["--pool-workers", "4", "--vram-reserve-mib", "4096"]);
        let out = launch_args(true, &tuned, &s(&["--pool-workers", "8"]));
        assert!(out.contains(&"--lazy".to_string()) && out.contains(&"8".to_string()));
        assert!(!out.contains(&"4".to_string()));
        assert!(out.contains(&"4096".to_string()));
    }

    #[test]
    fn api_key_comes_from_the_run_config_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("strata-keyed.json"), r#"{"api_key": " sk-test "}"#).unwrap();
        std::fs::write(dir.path().join("strata-open.json"), r#"{"api_key": ""}"#).unwrap();
        let s = Settings { strata_dir: dir.path().display().to_string(), ..Default::default() };
        assert_eq!(api_key_for(&s, "strata-keyed.json").as_deref(), Some("sk-test"));
        assert_eq!(api_key_for(&s, "strata-open.json"), None);
        assert_eq!(api_key_for(&s, "strata-missing.json"), None);
        assert_eq!(api_key_for(&s, "../strata-keyed.json"), None);
    }

    #[test]
    fn upstream_headers_swap_credentials_without_leaking_the_client_key() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer lh_client_session".parse().unwrap());
        h.insert("x-api-key", "lh_client_key".parse().unwrap());
        h.insert("cookie", "harness_session=abc".parse().unwrap());
        h.insert("anthropic-version", "2023-06-01".parse().unwrap());
        let out = upstream_headers(&h, Some("strata-key"));
        assert_eq!(out["authorization"].to_str().unwrap(), "Bearer strata-key");
        assert!(!out.contains_key("x-api-key") && !out.contains_key("cookie"));
        assert!(out.values().all(|v| !v.to_str().unwrap_or("").contains("lh_client")));
        assert!(out.contains_key("anthropic-version"));
        let open = upstream_headers(&h, None);
        assert!(!open.contains_key("authorization") && !open.contains_key("x-api-key"));
    }

    #[test]
    fn engine_status_parsing() {
        assert_eq!(parse_engine_status(&json!({"loaded": true, "auto_load": false})), (Some(true), Some(false)));
        assert_eq!(parse_engine_status(&json!({})), (None, None));
    }

    #[tokio::test]
    async fn passthrough_streams_sse_and_swaps_credentials() {
        let target = strata_target(serve(fake_messages()).await);
        let mut client = HeaderMap::new();
        client.insert("x-api-key", "lh_client".parse().unwrap());
        client.insert("authorization", "Bearer lh_client".parse().unwrap());
        let headers = upstream_headers(&client, Some("strata-key"));
        let body = Bytes::from_static(br#"{"model":"strata-a","max_tokens":4,"stream":true,"messages":[]}"#);
        let resp = proxy::relay(&target, Method::POST, "/v1/messages", headers, Some(reqwest::Body::from(body)), None).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"].to_str().unwrap(), "text/event-stream");
        assert_eq!(resp.headers()["x-seen-auth"].to_str().unwrap(), "Bearer strata-key");
        assert_eq!(resp.headers()["x-seen-xapi"].to_str().unwrap(), "false");
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&bytes[..], SSE.as_bytes());
    }

    #[tokio::test]
    async fn stream_keeps_the_server_busy_until_it_is_read() {
        let target = strata_target(serve(fake_messages()).await);
        let act = Activity::new();
        let headers = upstream_headers(&HeaderMap::new(), None);
        let body = Bytes::from_static(b"{}");
        let resp = proxy::relay(&target, Method::POST, "/v1/messages", headers, Some(reqwest::Body::from(body)), Some(act.begin())).await.unwrap();
        assert_eq!(act.snapshot().active, 1, "in flight while the body is pending");
        let _ = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert_eq!(act.snapshot().active, 0, "released once the stream ended");
    }

    #[tokio::test]
    async fn dropping_a_stream_releases_its_guard() {
        let act = Activity::new();
        let s = guard_stream(stream::iter(vec![1u8, 2, 3]), Some(act.begin()));
        assert_eq!(act.snapshot().active, 1);
        drop(s);
        assert_eq!(act.snapshot().active, 0);
        let s = guard_stream(stream::iter(vec![1u8, 2]), Some(act.begin()));
        let items: Vec<u8> = s.collect().await;
        assert_eq!(items, vec![1, 2]);
        assert_eq!(act.snapshot().active, 0);
    }

    #[tokio::test]
    async fn unload_maps_busy_409_and_auth_shape() {
        let busy = serve(fake_unload(true)).await;
        assert_eq!(control(&busy, "unload", Some("strata-key"), Duration::from_secs(5)).await.unwrap(), Control::Busy);
        let free = serve(fake_unload(false)).await;
        assert_eq!(control(&free, "unload", Some("strata-key"), Duration::from_secs(5)).await.unwrap(), Control::Done);
        // no key sent: the fake refuses, which surfaces as a rejection
        assert_eq!(control(&free, "unload", None, Duration::from_secs(5)).await.unwrap(), Control::Rejected(400));
        assert!(control("http://127.0.0.1:1", "unload", None, Duration::from_secs(5)).await.is_err());
    }

    #[test]
    fn busy_unload_is_a_409_with_detail() {
        let err = control_result(Ok(Control::Busy)).unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(!err.1.is_empty());
        assert_eq!(control_result(Ok(Control::Rejected(404))).unwrap_err().0, StatusCode::NOT_FOUND);
        assert_eq!(control_result(Err("connect".into())).unwrap_err().0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(control_result(Ok(Control::Done)).is_ok());
    }

    #[test]
    fn routes_merge_with_the_proxy_and_anthropic_routers() {
        let _ = Router::<AppState>::new().merge(router()).merge(proxy::router()).merge(crate::anthropic::router());
    }
}
