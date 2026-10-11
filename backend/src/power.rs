//! Mode switching (full / shared), time-based schedule and the Windows bridge.
//!
//! A switch does three things, serialised by a mutex:
//!   1. persist `settings.mode.current` and broadcast the resolved `ModeProfile` (watch channel),
//!   2. tell the Windows host (GPU power limit, vmmemWSL priority) through a scheduled task,
//!   3. re-pin / renice running servers and restart auto-tuned ones (`InstanceManager::apply_profile`).
//!
//! Windows bridge: the harness runs unprivileged inside WSL2, but `nvidia-smi -pl` needs
//! Windows administrator rights. `scripts/windows/setup-host.ps1` therefore registers the
//! scheduled task `\LLMHarness\ApplyMode` (run with highest privileges) which only ever runs
//! `C:\ProgramData\llm-harness\apply-mode.ps1`. We write the desired state to `mode.json` next to
//! it, start the task with `schtasks.exe /Run` and poll `mode-result.json` for the outcome.
//!
//! Routes: GET /api/mode, PUT /api/mode, PUT /api/mode/settings

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::{get, put};
use axum::{Json, Router};
use chrono::{Datelike, Local, NaiveDateTime, NaiveTime, TimeZone, Timelike, Weekday};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{watch, OnceCell};

use crate::config::{merge_json, SettingsStore};
use crate::error::{ApiError, ApiResult};
use crate::mode::{self, Mode, ModeProfile, Schedule};
use crate::state::AppState;
use crate::types::GpuInfo;

const TASK_NAME: &str = "\\LLMHarness\\ApplyMode";
const FALLBACK_PROGRAM_DATA: &str = "/mnt/c/ProgramData";
const MODE_FILE: &str = "mode.json";
const RESULT_FILE: &str = "mode-result.json";
/// Lower bound the Windows script accepts (`nvidia-smi -pl`); keep both sides in sync.
const MIN_POWER_LIMIT_W: u32 = 50;
const RESULT_POLL_TIMEOUT: Duration = Duration::from_secs(8);
const SCHEDULE_INTERVAL: Duration = Duration::from_secs(30);
const AVAILABILITY_TTL: Duration = Duration::from_secs(20);

pub const MSG_BRIDGE_MISSING: &str =
    "Windows 側で scripts/windows/setup-host.ps1 を実行すると GPU 電力制限と vmmem 優先度も切り替わります";
pub const MSG_NOT_WSL: &str =
    "WSL 上で動作していないため、GPU 電力制限と vmmem 優先度の切り替えは行われません (CPU/ダウンロード制限のみ適用)";

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// "HH:MM" -> minutes since midnight.
fn parse_hhmm(s: &str) -> Option<u32> {
    let t = NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()?;
    Some(t.hour() * 60 + t.minute())
}

/// Which mode the schedule asks for at `now` (local time). Ignores `schedule.enabled`
/// (the caller decides whether the schedule applies at all).
///
/// The full window `[full_start, full_end)` may wrap past midnight (20:00 -> 08:00). With
/// `weekends_full`, Saturday and Sunday are full all day. Invalid or equal start/end times mean
/// the full window is disabled (everything outside weekends is shared).
pub fn scheduled_mode(schedule: &Schedule, now: NaiveDateTime) -> Mode {
    if schedule.weekends_full && matches!(now.weekday(), Weekday::Sat | Weekday::Sun) {
        return Mode::Full;
    }
    let (Some(start), Some(end)) = (parse_hhmm(&schedule.full_start), parse_hhmm(&schedule.full_end)) else {
        return Mode::Shared;
    };
    let t = now.hour() * 60 + now.minute();
    let in_full = if start < end {
        t >= start && t < end
    } else if start > end {
        t >= start || t < end
    } else {
        false
    };
    if in_full {
        Mode::Full
    } else {
        Mode::Shared
    }
}

/// First minute boundary after `now` (within the next 8 days) at which the scheduled mode differs.
pub fn next_schedule_change(schedule: &Schedule, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let current = scheduled_mode(schedule, now);
    let mut t = now.with_second(0)?.with_nanosecond(0)? + chrono::Duration::minutes(1);
    for _ in 0..(8 * 24 * 60) {
        if scheduled_mode(schedule, t) != current {
            return Some(t);
        }
        t += chrono::Duration::minutes(1);
    }
    None
}

/// GPU power limit in watts for `percent` of the board default; `None` = leave at / reset to default.
pub fn power_limit_w(default_limit_w: Option<f64>, percent: u32) -> Option<u32> {
    let default = default_limit_w.filter(|d| d.is_finite() && *d > 0.0)?;
    if percent >= 100 {
        return None;
    }
    let w = (default * f64::from(percent) / 100.0).round();
    Some((w as u32).max(MIN_POWER_LIMIT_W))
}

/// Desired Windows-side state (`mode.json`).
pub fn desired_state(profile: &ModeProfile, gpus: &[GpuInfo], request_id: &str, requested_at: &str) -> Value {
    let gpu_entries: Vec<Value> = gpus
        .iter()
        .map(|g| json!({ "index": g.index, "power_limit_w": power_limit_w(g.power_default_limit, profile.gpu_power_percent) }))
        .collect();
    json!({
        "mode": profile.mode,
        "gpus": gpu_entries,
        "vmmem_priority": profile.vmmem_priority,
        "request_id": request_id,
        "requested_at": requested_at,
    })
}

fn local_iso(dt: NaiveDateTime) -> Option<String> {
    Local.from_local_datetime(&dt).earliest().map(|d| d.to_rfc3339())
}

fn valid_hhmm(s: &str) -> bool {
    parse_hhmm(s).is_some()
}

// ---------------------------------------------------------------------------------------------
// Interop helpers
// ---------------------------------------------------------------------------------------------

/// Run a (Windows interop) command with a timeout. `None` on spawn failure / timeout.
async fn run_cmd(program: &str, args: &[&str], timeout: Duration) -> Option<(bool, String)> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // cmd.exe complains when started from a WSL path; start from a Windows drive if we have one.
    if Path::new("/mnt/c").is_dir() {
        cmd.current_dir("/mnt/c");
    }
    let out = tokio::time::timeout(timeout, cmd.output()).await.ok()?.ok()?;
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

async fn locate_bridge_dir() -> PathBuf {
    let base = async {
        let (ok, out) = run_cmd("cmd.exe", &["/c", "echo %ProgramData%"], Duration::from_secs(5)).await?;
        let win = out.trim().to_string();
        if !ok || win.is_empty() || win.contains('%') {
            return None;
        }
        let (ok, p) = run_cmd("wslpath", &["-u", &win], Duration::from_secs(5)).await?;
        let p = p.trim();
        (ok && !p.is_empty()).then(|| PathBuf::from(p))
    }
    .await
    .unwrap_or_else(|| PathBuf::from(FALLBACK_PROGRAM_DATA));
    base.join("llm-harness")
}

// ---------------------------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------------------------

/// Outcome of one bridge attempt (`bridge.last_result` in the API).
#[derive(Clone, Debug, Serialize)]
pub struct BridgeResult {
    pub ok: bool,
    pub message: String,
    pub messages: Vec<String>,
    pub mode: Mode,
    pub requested_at: String,
    pub applied_at: Option<String>,
}

#[derive(Clone, Debug)]
struct Availability {
    available: bool,
    message: Option<String>,
}

struct Status {
    source: String,
    switched_at: String,
    restarted: Vec<String>,
    last_result: Option<BridgeResult>,
}

pub struct PowerManager {
    settings: Arc<SettingsStore>,
    mode_tx: watch::Sender<ModeProfile>,
    switch_lock: tokio::sync::Mutex<()>,
    status: Mutex<Status>,
    /// Mode the schedule requested last time it was evaluated (None = not evaluated yet).
    last_scheduled: Mutex<Option<Mode>>,
    bridge_dir: OnceCell<PathBuf>,
    availability: Mutex<Option<(Instant, Availability)>>,
}

impl PowerManager {
    pub fn new(settings: Arc<SettingsStore>, mode_tx: watch::Sender<ModeProfile>) -> Arc<Self> {
        Arc::new(Self {
            settings,
            mode_tx,
            switch_lock: tokio::sync::Mutex::new(()),
            status: Mutex::new(Status {
                source: "startup".into(),
                switched_at: Local::now().to_rfc3339(),
                restarted: vec![],
                last_result: None,
            }),
            last_scheduled: Mutex::new(None),
            bridge_dir: OnceCell::new(),
            availability: Mutex::new(None),
        })
    }

    /// Startup apply + schedule loop; never returns.
    pub async fn run(self: Arc<Self>, state: AppState) {
        // GPU power limits are reset by a reboot / driver reload: re-apply the persisted mode.
        {
            let _guard = self.switch_lock.lock().await;
            let profile = state.profile();
            let result = self.apply_bridge(&state, &profile).await;
            let mut st = self.status.lock();
            st.source = "startup".into();
            st.switched_at = Local::now().to_rfc3339();
            st.last_result = result;
        }
        let mut tick = tokio::time::interval(SCHEDULE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.evaluate_schedule(&state).await;
        }
    }

    /// Switch when the schedule crossed a window boundary since the last evaluation. A manual
    /// switch therefore holds until the next boundary.
    pub async fn evaluate_schedule(&self, state: &AppState) {
        let schedule = self.settings.get().mode.schedule;
        if !schedule.enabled {
            *self.last_scheduled.lock() = None;
            return;
        }
        let wanted = scheduled_mode(&schedule, Local::now().naive_local());
        let changed = {
            let mut last = self.last_scheduled.lock();
            let changed = *last != Some(wanted);
            *last = Some(wanted);
            changed
        };
        if changed && state.profile().mode != wanted {
            tracing::info!("schedule: switching to {wanted:?}");
            if let Err(e) = self.set_mode(state, wanted, "schedule").await {
                tracing::warn!("schedule switch failed: {e:#}");
            }
        }
    }

    pub async fn set_mode(&self, state: &AppState, mode: Mode, source: &str) -> anyhow::Result<()> {
        let _guard = self.switch_lock.lock().await;
        let settings = self.settings.modify(|s| s.mode.current = mode)?;
        let profile = mode::resolve(mode, &settings, crate::topology::cpu_topology());
        self.mode_tx.send_replace(profile.clone());
        tracing::info!(
            "mode -> {mode:?} (source: {source}, cpus: {}, gpu power: {}%, nice: {})",
            profile.logical_cpus,
            profile.gpu_power_percent,
            profile.nice
        );
        let (bridge, restarted) = tokio::join!(
            self.apply_bridge(state, &profile),
            state.instances.apply_profile(state, &profile, settings.mode.auto_restart)
        );
        if let Some(b) = &bridge {
            if b.ok {
                tracing::info!("windows bridge: {}", b.message);
            } else {
                tracing::warn!("windows bridge: {}", b.message);
            }
        }
        if !restarted.is_empty() {
            tracing::info!("restarted instances: {}", restarted.join(", "));
        }
        let mut st = self.status.lock();
        st.source = source.to_string();
        st.switched_at = Local::now().to_rfc3339();
        st.restarted = restarted;
        st.last_result = bridge;
        Ok(())
    }

    // ---- Windows bridge ----

    async fn bridge_dir(&self) -> &Path {
        self.bridge_dir.get_or_init(locate_bridge_dir).await.as_path()
    }

    async fn probe_bridge(&self) -> Availability {
        if !crate::system::is_wsl() {
            return Availability { available: false, message: Some(MSG_NOT_WSL.into()) };
        }
        let dir = self.bridge_dir().await;
        let dir_ok = tokio::fs::metadata(dir).await.map(|m| m.is_dir()).unwrap_or(false);
        let task_ok = matches!(
            run_cmd("schtasks.exe", &["/Query", "/TN", TASK_NAME], Duration::from_secs(8)).await,
            Some((true, _))
        );
        if dir_ok && task_ok {
            Availability { available: true, message: None }
        } else {
            Availability { available: false, message: Some(MSG_BRIDGE_MISSING.into()) }
        }
    }

    async fn availability(&self, fresh: bool) -> Availability {
        if !fresh {
            if let Some((at, a)) = &*self.availability.lock() {
                if at.elapsed() < AVAILABILITY_TTL {
                    return a.clone();
                }
            }
        }
        let a = self.probe_bridge().await;
        *self.availability.lock() = Some((Instant::now(), a.clone()));
        a
    }

    /// Push the desired Windows-side state. `None` when the bridge is unavailable.
    async fn apply_bridge(&self, state: &AppState, profile: &ModeProfile) -> Option<BridgeResult> {
        if !self.availability(true).await.available {
            return None;
        }
        let dir = self.bridge_dir().await.to_path_buf();
        let requested_at = Local::now().to_rfc3339();
        let request_id = format!("{}-{}", chrono::Utc::now().timestamp_millis(), std::process::id());
        let desired = desired_state(profile, &state.telemetry.gpus(), &request_id, &requested_at);
        let fail = |message: String| BridgeResult {
            ok: false,
            message,
            messages: vec![],
            mode: profile.mode,
            requested_at: requested_at.clone(),
            applied_at: None,
        };

        // Written in place: setup-host.ps1 grants the user Modify on mode.json only (not on the folder).
        let body = match serde_json::to_vec_pretty(&desired) {
            Ok(b) => b,
            Err(e) => return Some(fail(format!("mode.json を作れません: {e}"))),
        };
        if let Err(e) = tokio::fs::write(dir.join(MODE_FILE), body).await {
            return Some(fail(format!("{} に書き込めません: {e}", dir.join(MODE_FILE).display())));
        }
        match run_cmd("schtasks.exe", &["/Run", "/TN", TASK_NAME], Duration::from_secs(10)).await {
            Some((true, _)) => {}
            Some((false, _)) => return Some(fail("タスク \\LLMHarness\\ApplyMode を起動できませんでした".into())),
            None => return Some(fail("schtasks.exe の実行がタイムアウトまたは失敗しました".into())),
        }

        let result_path = dir.join(RESULT_FILE);
        let deadline = Instant::now() + RESULT_POLL_TIMEOUT;
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&result_path).await {
                if let Ok(v) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
                    if v.get("request_id").and_then(Value::as_str) == Some(request_id.as_str()) {
                        let messages: Vec<String> = v
                            .get("messages")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_string)).collect())
                            .unwrap_or_default();
                        let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
                        return Some(BridgeResult {
                            ok,
                            message: if ok { "Windows 側へ反映しました".into() } else { "Windows 側の反映で一部失敗しました".into() },
                            messages,
                            mode: profile.mode,
                            requested_at,
                            applied_at: v.get("applied_at").and_then(Value::as_str).map(str::to_string),
                        });
                    }
                }
            }
            if Instant::now() >= deadline {
                return Some(fail("Windows 側の結果を待ってタイムアウトしました (反映は続行中の可能性があります)".into()));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    // ---- API ----

    /// JSON served by GET /api/mode (and returned by the PUT endpoints).
    pub async fn snapshot(&self, state: &AppState) -> Value {
        let settings = self.settings.get();
        let profile = state.profile();
        let avail = self.availability(false).await;
        let now = Local::now().naive_local();
        let schedule = &settings.mode.schedule;
        let (scheduled, next_switch) = if schedule.enabled {
            (
                Some(scheduled_mode(schedule, now)),
                next_schedule_change(schedule, now).and_then(local_iso),
            )
        } else {
            (None, None)
        };
        let topo = crate::topology::cpu_topology();
        let st = self.status.lock();
        json!({
            "mode": profile.mode,
            "source": st.source,
            // unix seconds (what the frontend formats)
            "switched_at": iso_to_unix(&st.switched_at),
            "profile": profile,
            "settings": settings.mode,
            "scheduled_mode": scheduled,
            "next_switch_at": next_switch.as_deref().and_then(iso_to_unix),
            "bridge": {
                "available": avail.available,
                "message": avail.message,
                "last_result": st.last_result,
            },
            "restarted": st.restarted,
            "topology": { "physical_cores": topo.physical_cores, "logical_cores": topo.logical_cores },
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/mode", get(get_mode).put(put_mode))
        .route("/api/mode/settings", put(put_settings))
}

async fn get_mode(State(state): State<AppState>) -> ApiResult<Value> {
    Ok(Json(state.power.snapshot(&state).await))
}

#[derive(Deserialize)]
struct SetModeBody {
    mode: Mode,
}

async fn put_mode(State(state): State<AppState>, Json(body): Json<SetModeBody>) -> ApiResult<Value> {
    state.power.set_mode(&state, body.mode, "manual").await?;
    Ok(Json(state.power.snapshot(&state).await))
}

async fn put_settings(State(state): State<AppState>, Json(patch): Json<Value>) -> ApiResult<Value> {
    let Value::Object(mut patch_obj) = patch else {
        return Err(ApiError::bad_request("JSON オブジェクトを指定してください"));
    };
    // The active mode is changed through PUT /api/mode only.
    patch_obj.remove("current");
    let patch = Value::Object(patch_obj);

    let before = state.settings.get().mode;
    let mut merged = serde_json::to_value(&before).map_err(|e| ApiError::internal(e.to_string()))?;
    merge_json(&mut merged, &patch);
    let after: mode::ModeSettings =
        serde_json::from_value(merged.clone()).map_err(|e| ApiError::bad_request(format!("設定が不正です: {e}")))?;
    if !valid_hhmm(&after.schedule.full_start) || !valid_hhmm(&after.schedule.full_end) {
        return Err(ApiError::bad_request("schedule の時刻は HH:MM 形式で指定してください"));
    }
    if let Value::Object(m) = &mut merged {
        m.remove("current");
    }
    state.settings.update(&json!({ "mode": merged }))?;

    let current = state.profile().mode;
    let shared_changed = serde_json::to_value(&before.shared).ok() != serde_json::to_value(&after.shared).ok();
    if shared_changed && current == Mode::Shared {
        state.power.set_mode(&state, current, "settings").await?;
    }
    // Re-evaluate from scratch so enabling / editing the schedule takes effect immediately.
    *state.power.last_scheduled.lock() = None;
    state.power.evaluate_schedule(&state).await;
    Ok(Json(state.power.snapshot(&state).await))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn sched(start: &str, end: &str, weekends: bool) -> Schedule {
        Schedule { enabled: true, full_start: start.into(), full_end: end.into(), weekends_full: weekends }
    }

    // 2026-10-12 is a Monday.
    fn at(day: u32, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn wrap_window_overnight() {
        let s = sched("20:00", "08:00", false);
        assert_eq!(scheduled_mode(&s, at(12, 7, 59)), Mode::Full);
        assert_eq!(scheduled_mode(&s, at(12, 8, 0)), Mode::Shared);
        assert_eq!(scheduled_mode(&s, at(12, 12, 0)), Mode::Shared);
        assert_eq!(scheduled_mode(&s, at(12, 19, 59)), Mode::Shared);
        assert_eq!(scheduled_mode(&s, at(12, 20, 0)), Mode::Full);
        assert_eq!(scheduled_mode(&s, at(12, 23, 59)), Mode::Full);
        assert_eq!(scheduled_mode(&s, at(13, 0, 0)), Mode::Full);
    }

    #[test]
    fn non_wrap_window() {
        let s = sched("12:00", "13:30", false);
        assert_eq!(scheduled_mode(&s, at(12, 11, 59)), Mode::Shared);
        assert_eq!(scheduled_mode(&s, at(12, 12, 0)), Mode::Full);
        assert_eq!(scheduled_mode(&s, at(12, 13, 29)), Mode::Full);
        assert_eq!(scheduled_mode(&s, at(12, 13, 30)), Mode::Shared);
    }

    #[test]
    fn weekends_are_full_all_day() {
        let s = sched("20:00", "08:00", true);
        assert_eq!(scheduled_mode(&s, at(17, 12, 0)), Mode::Full); // Saturday noon
        assert_eq!(scheduled_mode(&s, at(18, 15, 0)), Mode::Full); // Sunday afternoon
        assert_eq!(scheduled_mode(&s, at(19, 12, 0)), Mode::Shared); // Monday noon
        let s = sched("20:00", "08:00", false);
        assert_eq!(scheduled_mode(&s, at(17, 12, 0)), Mode::Shared);
    }

    #[test]
    fn invalid_times_disable_full_window() {
        assert_eq!(scheduled_mode(&sched("25:00", "08:00", false), at(12, 3, 0)), Mode::Shared);
        assert_eq!(scheduled_mode(&sched("20:00", "abc", false), at(12, 21, 0)), Mode::Shared);
        assert_eq!(scheduled_mode(&sched("", "", false), at(12, 21, 0)), Mode::Shared);
        // Weekend rule still applies.
        assert_eq!(scheduled_mode(&sched("x", "y", true), at(17, 21, 0)), Mode::Full);
        // Empty window.
        assert_eq!(scheduled_mode(&sched("08:00", "08:00", false), at(12, 8, 0)), Mode::Shared);
    }

    #[test]
    fn next_change_finds_boundary() {
        let s = sched("20:00", "08:00", false);
        assert_eq!(next_schedule_change(&s, at(12, 12, 0)), Some(at(12, 20, 0)));
        assert_eq!(next_schedule_change(&s, at(12, 23, 0)), Some(at(13, 8, 0)));
        // Friday evening with weekends_full: full stays on until Monday 08:00.
        let s = sched("20:00", "08:00", true);
        assert_eq!(next_schedule_change(&s, at(16, 21, 0)), Some(at(19, 8, 0)));
        // Never changes: invalid window, no weekends -> always shared.
        assert_eq!(next_schedule_change(&sched("x", "y", false), at(12, 12, 0)), None);
    }

    #[test]
    fn power_limit_computation() {
        assert_eq!(power_limit_w(Some(300.0), 70), Some(210));
        assert_eq!(power_limit_w(Some(450.0), 70), Some(315));
        assert_eq!(power_limit_w(Some(300.0), 100), None);
        assert_eq!(power_limit_w(Some(300.0), 120), None);
        assert_eq!(power_limit_w(None, 70), None);
        assert_eq!(power_limit_w(Some(0.0), 70), None);
        assert_eq!(power_limit_w(Some(f64::NAN), 70), None);
        assert_eq!(power_limit_w(Some(100.0), 30), Some(50)); // clamped to the script's minimum
        assert_eq!(power_limit_w(Some(285.0), 70), Some(200)); // 199.5 rounds up
    }

    #[test]
    fn desired_state_shape() {
        let topo = crate::types::CpuTopology {
            model: "x".into(),
            physical_cores: 2,
            logical_cores: 4,
            cores: vec![vec![0, 2], vec![1, 3]],
        };
        let settings = crate::config::Settings::default();
        let profile = mode::resolve(Mode::Shared, &settings, &topo);
        let gpus = vec![
            GpuInfo { index: 0, power_default_limit: Some(300.0), ..Default::default() },
            GpuInfo { index: 1, power_default_limit: None, ..Default::default() },
        ];
        let v = desired_state(&profile, &gpus, "rid", "2026-10-12T09:00:00+09:00");
        assert_eq!(v["mode"], "shared");
        assert_eq!(v["vmmem_priority"], "BelowNormal");
        assert_eq!(v["request_id"], "rid");
        assert_eq!(v["gpus"][0]["index"], 0);
        assert_eq!(v["gpus"][0]["power_limit_w"], 210);
        assert!(v["gpus"][1]["power_limit_w"].is_null());

        let full = mode::resolve(Mode::Full, &settings, &topo);
        let v = desired_state(&full, &gpus, "rid", "t");
        assert_eq!(v["mode"], "full");
        assert_eq!(v["vmmem_priority"], "Normal");
        assert!(v["gpus"][0]["power_limit_w"].is_null());
    }

    #[test]
    fn hhmm_parsing() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("08:30"), Some(510));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("8"), None);
    }
}

fn iso_to_unix(s: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis() as f64 / 1000.0)
}
