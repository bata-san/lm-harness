//! Running inference servers, saved launch profiles, setup jobs and Ollama passthrough.
//!
//! Routes: /api/instances*, /api/profiles*, /api/runtimes, /api/jobs*, /api/ollama/*

use std::collections::{BTreeMap, HashSet};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::future::join_all;
use parking_lot::Mutex;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};

use crate::config::{read_json, write_json, SettingsStore};
use crate::error::{ok, ApiError, ApiResult};
use crate::mode::{Mode, ModeProfile};
use crate::procs::{LogTail, Proc, Status};
use crate::state::AppState;
use crate::{affinity, models, runtimes, tuning};

const MONITOR_INTERVAL: Duration = Duration::from_millis(1500);
const OLLAMA_DEFAULT_PORT: u16 = 11435;

// ---------------------------------------------------------------------- specs
fn default_runtime() -> String {
    "strata".into()
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaunchSpec {
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Model id under models_dir (llama.cpp / vllm) or empty for ollama.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub autostart: bool,
    /// Derive hardware-aware default flags and let `args` override them.
    #[serde(default = "default_true")]
    pub auto_tune: bool,
}

impl Default for LaunchSpec {
    fn default() -> Self {
        Self { name: String::new(), runtime: default_runtime(), model: String::new(), host: None, port: None, args: vec![], env: BTreeMap::new(), autostart: false, auto_tune: true }
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn id_or_new<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.filter(|s| !s.is_empty()).unwrap_or_else(new_id))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default = "new_id", deserialize_with = "id_or_new")]
    pub id: String,
    #[serde(flatten)]
    pub spec: LaunchSpec,
}

fn mode_str(m: Mode) -> &'static str {
    match m {
        Mode::Full => "full",
        Mode::Shared => "shared",
    }
}

fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

// ------------------------------------------------------------------ instances
#[derive(Default)]
struct Live {
    ready: bool,
    metrics: BTreeMap<String, f64>,
}

/// A launched inference server.
pub struct Instance {
    pub proc: Arc<Proc>,
    pub spec: LaunchSpec,
    pub runtime: String,
    pub model: String,
    pub host: String,
    pub port: u16,
    /// Mode the server was launched (and tuned) under.
    pub mode: Mode,
    live: Mutex<Live>,
}

impl Instance {
    pub fn id(&self) -> &str {
        &self.proc.id
    }

    pub fn alive(&self) -> bool {
        self.proc.alive()
    }

    /// Health check has passed (the server answers requests).
    pub fn ready(&self) -> bool {
        self.live.lock().ready
    }

    /// JSON the frontend's `Instance` type reads; `current` is the mode in force now.
    pub fn to_json(&self, current: Mode) -> Value {
        let alive = self.alive();
        let live = self.live.lock();
        let mut m = self.proc.to_json();
        m.insert("runtime".into(), json!(self.runtime));
        m.insert("model".into(), json!(self.model));
        m.insert("port".into(), json!(self.port));
        m.insert("host".into(), json!(self.host));
        m.insert("args".into(), json!(self.spec.args));
        m.insert("auto_tune".into(), json!(self.spec.auto_tune));
        m.insert("mode".into(), json!(mode_str(self.mode)));
        m.insert("stale".into(), json!(alive && self.mode != current));
        m.insert("ready".into(), json!(live.ready));
        m.insert("metrics".into(), json!(live.metrics));
        Value::Object(m)
    }
}

pub struct InstanceManager {
    settings: Arc<SettingsStore>,
    instances: Mutex<Vec<Arc<Instance>>>,
    jobs: Mutex<Vec<Arc<Proc>>>,
    profiles: Mutex<Vec<Profile>>,
    /// Serialises "pick a port -> register the instance" so concurrent launches never collide.
    launch_lock: tokio::sync::Mutex<()>,
}

impl InstanceManager {
    pub fn load(settings: Arc<SettingsStore>) -> Arc<Self> {
        let profiles: Vec<Profile> = read_json("profiles.json").unwrap_or_default();
        Arc::new(Self { settings, instances: Mutex::new(vec![]), jobs: Mutex::new(vec![]), profiles: Mutex::new(profiles), launch_lock: tokio::sync::Mutex::new(()) })
    }

    // -------------------------------------------------------------- profiles
    pub fn profiles(&self) -> Vec<Profile> {
        self.profiles.lock().clone()
    }

    pub fn save_profile(&self, p: Profile) -> anyhow::Result<Profile> {
        let mut list = self.profiles.lock();
        match list.iter_mut().find(|x| x.id == p.id) {
            Some(slot) => *slot = p.clone(),
            None => list.push(p.clone()),
        }
        write_json("profiles.json", &*list)?;
        Ok(p)
    }

    pub fn delete_profile(&self, id: &str) -> anyhow::Result<()> {
        let mut list = self.profiles.lock();
        list.retain(|p| p.id != id);
        write_json("profiles.json", &*list)
    }

    // ------------------------------------------------------------- instances
    pub fn get(&self, id: &str) -> Option<Arc<Instance>> {
        self.instances.lock().iter().find(|i| i.id() == id).cloned()
    }

    pub fn alive_instances(&self) -> Vec<Arc<Instance>> {
        self.instances.lock().iter().filter(|i| i.alive()).cloned().collect()
    }

    pub fn list(&self, current: Mode) -> Vec<Value> {
        let mut all: Vec<Arc<Instance>> = self.instances.lock().clone();
        all.sort_by(|a, b| b.proc.started_at.total_cmp(&a.proc.started_at));
        all.iter().map(|i| i.to_json(current)).collect()
    }

    pub async fn stop(&self, id: &str) -> bool {
        match self.get(id) {
            Some(i) => {
                i.proc.stop().await;
                true
            }
            None => false,
        }
    }

    /// Forget a finished instance (no-op while it is alive).
    pub fn remove(&self, id: &str) {
        self.instances.lock().retain(|i| i.id() != id || i.alive());
    }

    fn pick_port(&self, start: u16) -> u16 {
        let used: HashSet<u16> = self.alive_instances().iter().map(|i| i.port).collect();
        let mut port = start;
        while used.contains(&port) || !port_free(port) {
            port = port.saturating_add(1);
            if port == u16::MAX {
                break;
            }
        }
        port
    }

    /// Launch under the mode currently in force.
    pub async fn launch(&self, state: &AppState, spec: LaunchSpec) -> anyhow::Result<Arc<Instance>> {
        let profile = state.profile();
        self.launch_with(state, spec, &profile).await
    }

    async fn effective_args(&self, state: &AppState, spec: &LaunchSpec, profile: &ModeProfile) -> Vec<String> {
        if !spec.auto_tune {
            return spec.args.clone();
        }
        let tuned = match spec.runtime.as_str() {
            "llama.cpp" => {
                let dir = self.settings.get().models_dir();
                let id = spec.model.clone();
                let model = tokio::task::spawn_blocking(move || models::scan(&dir).into_iter().find(|m| m.id == id)).await.ok().flatten();
                let Some(model) = model else { return spec.args.clone() };
                let telemetry = state.telemetry.clone();
                let gpus = tokio::task::spawn_blocking(move || telemetry.gpus()).await.unwrap_or_default();
                tuning::recommend(&model, &gpus, profile, None, "q8_0", 1).args
            }
            "vllm" => {
                let telemetry = state.telemetry.clone();
                let gpus = tokio::task::spawn_blocking(move || telemetry.gpus()).await.unwrap_or_default();
                tuning::vllm_args(profile, &gpus)
            }
            _ => return spec.args.clone(),
        };
        tuning::merge_args(&tuned, &spec.args)
    }

    async fn launch_with(&self, state: &AppState, spec: LaunchSpec, profile: &ModeProfile) -> anyhow::Result<Arc<Instance>> {
        let settings = self.settings.get();
        let host = spec.host.clone().filter(|h| !h.is_empty()).unwrap_or_else(|| settings.default_host.clone());
        let model_path = if spec.model.is_empty() {
            String::new()
        } else if spec.runtime == "strata" {
            // Strata models are run configs inside its own checkout, not files under models_dir.
            if !runtimes::valid_strata_config(&spec.model) {
                anyhow::bail!("invalid Strata config {:?}", spec.model);
            }
            spec.model.clone()
        } else {
            models::resolve(&settings.models_dir(), &spec.model)?.to_string_lossy().into_owned()
        };
        if spec.runtime != "ollama" && model_path.is_empty() {
            anyhow::bail!("モデルを選択してください");
        }
        let args = self.effective_args(state, &spec, profile).await;

        let _guard = self.launch_lock.lock().await;
        let port = match spec.port.filter(|p| *p != 0) {
            Some(p) => p,
            None if spec.runtime == "ollama" => OLLAMA_DEFAULT_PORT,
            None => self.pick_port(settings.port_range_start),
        };
        if !port_free(port) || self.alive_instances().iter().any(|i| i.port == port) {
            anyhow::bail!("port {port} は使用中です");
        }
        let (cmd, mut env) = runtimes::build_command(&spec.runtime, &model_path, &host, port, &args, &settings)?;
        env.extend(spec.env.clone());
        let name = if !spec.name.is_empty() {
            spec.name.clone()
        } else if !spec.model.is_empty() {
            spec.model.rsplit('/').next().unwrap_or(&spec.model).to_string()
        } else {
            spec.runtime.clone()
        };
        let proc = Proc::new(name, cmd, env, None, "instance", Map::new()).pinned(profile.cpu_set.clone(), profile.nice).privacy(settings.privacy_mode).start().await;
        let inst = Arc::new(Instance {
            proc,
            runtime: spec.runtime.clone(),
            model: spec.model.clone(),
            host,
            port,
            mode: profile.mode,
            spec,
            live: Mutex::new(Live::default()),
        });
        self.instances.lock().push(inst.clone());
        Ok(inst)
    }

    pub async fn stop_all(&self) {
        let procs: Vec<Arc<Proc>> = self.alive_instances().into_iter().map(|i| i.proc.clone()).chain(self.jobs.lock().iter().filter(|j| j.alive()).cloned()).collect();
        join_all(procs.iter().map(|p| p.stop())).await;
    }

    // ------------------------------------------------------------------ jobs
    pub fn job_runs(&self) -> Vec<Value> {
        let mut all: Vec<Arc<Proc>> = self.jobs.lock().clone();
        all.sort_by(|a, b| b.started_at.total_cmp(&a.started_at));
        all.iter().map(|j| Value::Object(j.to_json())).collect()
    }

    pub fn job_run(&self, id: &str) -> Option<Arc<Proc>> {
        self.jobs.lock().iter().find(|j| j.id == id).cloned()
    }

    /// Start a setup job, or return the run already in progress for it.
    pub async fn run_job(&self, state: &AppState, job_id: &str) -> anyhow::Result<Arc<Proc>> {
        let Some(def) = runtimes::job(job_id) else { anyhow::bail!("unknown job") };
        if let Some(j) = self.jobs.lock().iter().find(|j| j.alive() && j.meta.get("job").and_then(Value::as_str) == Some(job_id)) {
            return Ok(j.clone());
        }
        let settings = self.settings.get();
        let (cmd, env) = runtimes::job_command(job_id, &settings)?;
        let profile = state.profile();
        let meta = Map::from_iter([("job".to_string(), json!(job_id))]);
        let proc = Proc::new(def.title, cmd, env, None, "job", meta).pinned(profile.cpu_set.clone(), profile.nice).privacy(settings.privacy_mode).start().await;
        proc.set_alive_status(Status::Running);
        self.jobs.lock().push(proc.clone());
        // versions change after an install/build finishes
        let watched = proc.clone();
        tokio::spawn(async move {
            while watched.alive() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            runtimes::invalidate_cache().await;
        });
        Ok(proc)
    }

    // ------------------------------------------------------------ monitoring
    /// Health/metrics loop; never returns.
    pub async fn monitor(self: Arc<Self>, _state: AppState) {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).build().unwrap_or_default();
        loop {
            let all: Vec<Arc<Instance>> = self.instances.lock().clone();
            join_all(all.iter().map(|i| check_instance(&client, i))).await;
            tokio::time::sleep(MONITOR_INTERVAL).await;
        }
    }

    pub async fn autostart(&self, state: &AppState) {
        for prof in self.profiles() {
            if prof.spec.autostart {
                if let Err(e) = self.launch(state, prof.spec.clone()).await {
                    tracing::warn!("autostart {} failed: {e:#}", prof.spec.name);
                }
            }
        }
    }

    /// Apply a new mode: re-pin/renice running servers live; if `restart_auto_tuned`, restart
    /// servers launched with auto_tune so their args match the new limits. Returns restarted ids.
    pub async fn apply_profile(&self, state: &AppState, profile: &ModeProfile, restart_auto_tuned: bool) -> Vec<String> {
        let alive = self.alive_instances();
        let jobs: Vec<Arc<Proc>> = self.jobs.lock().iter().filter(|j| j.alive()).cloned().collect();
        let pids: Vec<i32> = alive.iter().filter_map(|i| i.proc.alive_pid()).chain(jobs.iter().filter_map(|j| j.alive_pid())).map(|p| p as i32).collect();
        join_all(pids.into_iter().map(|pid| {
            let cpus = profile.cpu_set.clone();
            let nice = profile.nice;
            tokio::task::spawn_blocking(move || affinity::apply_tree(pid, &cpus, nice))
        }))
        .await;

        if !restart_auto_tuned {
            return vec![];
        }
        let targets: Vec<Arc<Instance>> = alive.into_iter().filter(|i| i.spec.auto_tune && matches!(i.runtime.as_str(), "llama.cpp" | "vllm") && i.mode != profile.mode).collect();
        join_all(targets.iter().map(|inst| self.restart(state, inst, profile))).await.into_iter().flatten().collect()
    }

    /// Stop an instance and relaunch the same spec on the same port under `profile`.
    async fn restart(&self, state: &AppState, inst: &Arc<Instance>, profile: &ModeProfile) -> Option<String> {
        inst.proc.log(format!("[harness] モード切替のため再起動します ({} → {})", mode_str(inst.mode), mode_str(profile.mode)));
        inst.proc.stop().await;
        // the old server's socket may take a moment to go away
        for _ in 0..40 {
            if port_free(inst.port) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let mut spec = inst.spec.clone();
        spec.port = Some(inst.port);
        spec.host = Some(inst.host.clone());
        match self.launch_with(state, spec, profile).await {
            Ok(new) => Some(new.id().to_string()),
            Err(e) => {
                inst.proc.log(format!("[harness] 再起動失敗: {e:#}"));
                tracing::warn!("restart of {} failed: {e:#}", inst.proc.name);
                None
            }
        }
    }
}

async fn check_instance(client: &reqwest::Client, inst: &Instance) {
    if !inst.alive() {
        inst.live.lock().ready = false;
        return;
    }
    let base = format!("http://127.0.0.1:{}", inst.port);
    let ready = matches!(client.get(format!("{base}{}", runtimes::health_path(&inst.runtime))).send().await, Ok(r) if r.status().is_success());
    let first_ready = {
        let mut live = inst.live.lock();
        let first = ready && !live.ready;
        live.ready = ready;
        first
    };
    if first_ready {
        inst.proc.log(format!("[harness] ready: http://localhost:{}/v1", inst.port));
    }
    inst.proc.set_alive_status(if ready { Status::Running } else { Status::Starting });
    if ready && matches!(inst.runtime.as_str(), "llama.cpp" | "vllm" | "strata") {
        if let Ok(r) = client.get(format!("{base}{}", runtimes::metrics_path(&inst.runtime))).header("Accept", "text/plain").send().await {
            if r.status().is_success() {
                if let Ok(text) = r.text().await {
                    inst.live.lock().metrics = runtimes::parse_metrics(&text);
                }
            }
        }
    }
}

// --------------------------------------------------------------------- routes
#[derive(Deserialize)]
struct SinceQuery {
    #[serde(default)]
    since: u64,
}

const TAIL_LIMIT: usize = 1000;

async fn list_instances(State(st): State<AppState>) -> Json<Vec<Value>> {
    Json(st.instances.list(st.profile().mode))
}

async fn launch_instance(State(st): State<AppState>, Json(spec): Json<LaunchSpec>) -> ApiResult<Value> {
    let inst = st.instances.launch(&st, spec).await.map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(inst.to_json(st.profile().mode)))
}

async fn stop_instance(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    if !st.instances.stop(&id).await {
        return Err(ApiError::not_found("not found"));
    }
    ok()
}

async fn remove_instance(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    st.instances.remove(&id);
    ok()
}

async fn instance_logs(State(st): State<AppState>, Path(id): Path<String>, Query(q): Query<SinceQuery>) -> ApiResult<LogTail> {
    let inst = st.instances.get(&id).ok_or_else(|| ApiError::not_found("not found"))?;
    Ok(Json(inst.proc.tail(q.since, TAIL_LIMIT)))
}

async fn list_profiles(State(st): State<AppState>) -> Json<Vec<Profile>> {
    Json(st.instances.profiles())
}

async fn save_profile(State(st): State<AppState>, Json(p): Json<Profile>) -> ApiResult<Profile> {
    Ok(Json(st.instances.save_profile(p)?))
}

async fn delete_profile(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    st.instances.delete_profile(&id)?;
    ok()
}

async fn list_runtimes(State(st): State<AppState>) -> Json<Vec<runtimes::RuntimeStatus>> {
    Json(runtimes::status(&st.settings.get()).await)
}

async fn ollama_models(State(st): State<AppState>) -> Json<Value> {
    let s = st.settings.get();
    let t = Duration::from_secs(3);
    let (tags, ps) = tokio::join!(runtimes::ollama_api(&s, "/api/tags", reqwest::Method::GET, None, t), runtimes::ollama_api(&s, "/api/ps", reqwest::Method::GET, None, t));
    let list = |v: &Option<Value>| v.as_ref().and_then(|v| v.get("models")).cloned().unwrap_or_else(|| json!([]));
    Json(json!({ "available": tags.is_some(), "models": list(&tags), "running": list(&ps) }))
}

async fn ollama_action(State(st): State<AppState>, Path(action): Path<String>, Json(body): Json<Value>) -> ApiResult<Value> {
    let s = st.settings.get();
    let model = body.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let r = match action.as_str() {
        "load" => runtimes::ollama_api(&s, "/api/generate", reqwest::Method::POST, Some(json!({"model": model, "keep_alive": "30m"})), Duration::from_secs(600)).await,
        "unload" => runtimes::ollama_api(&s, "/api/generate", reqwest::Method::POST, Some(json!({"model": model, "keep_alive": 0})), Duration::from_secs(60)).await,
        "delete" => runtimes::ollama_api(&s, "/api/delete", reqwest::Method::DELETE, Some(json!({"model": model})), Duration::from_secs(60)).await,
        _ => return Err(ApiError::bad_request("bad action")),
    };
    if r.is_none() {
        return Err(ApiError::bad_gateway("ollama request failed"));
    }
    ok()
}

async fn list_jobs(State(st): State<AppState>) -> Json<Value> {
    let available: Vec<Value> = runtimes::JOBS.iter().map(|j| json!({ "id": j.id, "title": j.title, "script": j.script, "sudo": j.sudo, "command": format!("bash scripts/{}", j.script) })).collect();
    Json(json!({ "available": available, "runs": st.instances.job_runs() }))
}

async fn run_job(State(st): State<AppState>, Path(job_id): Path<String>) -> ApiResult<Value> {
    let Some(def) = runtimes::job(&job_id) else { return Err(ApiError::not_found("unknown job")) };
    if def.sudo {
        return Err(ApiError::bad_request("sudo が必要なためターミナルで実行してください"));
    }
    let run = st.instances.run_job(&st, &job_id).await.map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(Value::Object(run.to_json())))
}

async fn job_logs(State(st): State<AppState>, Path(id): Path<String>, Query(q): Query<SinceQuery>) -> ApiResult<LogTail> {
    let run = st.instances.job_run(&id).ok_or_else(|| ApiError::not_found("not found"))?;
    Ok(Json(run.tail(q.since, TAIL_LIMIT)))
}

async fn stop_job(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    let run = st.instances.job_run(&id).ok_or_else(|| ApiError::not_found("not found"))?;
    run.stop().await;
    ok()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/instances", get(list_instances).post(launch_instance))
        .route("/api/instances/{id}/stop", post(stop_instance))
        .route("/api/instances/{id}", delete(remove_instance))
        .route("/api/instances/{id}/logs", get(instance_logs))
        .route("/api/profiles", get(list_profiles).post(save_profile))
        .route("/api/profiles/{id}", delete(delete_profile))
        .route("/api/runtimes", get(list_runtimes))
        .route("/api/ollama/models", get(ollama_models))
        .route("/api/ollama/{action}", post(ollama_action))
        .route("/api/jobs", get(list_jobs))
        .route("/api/jobs/{job_id}", post(run_job))
        .route("/api/jobs/runs/{id}/logs", get(job_logs))
        .route("/api/jobs/runs/{id}/stop", post(stop_job))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrip_and_defaults() {
        let p: Profile = serde_json::from_value(json!({"name": "a", "runtime": "vllm", "model": "m", "args": ["--x"], "env": {"K": "V"}, "autostart": true, "port": null})).unwrap();
        assert_eq!(p.id.len(), 8);
        assert!(p.spec.auto_tune);
        assert_eq!(p.spec.port, None);
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["id"], json!(p.id));
        assert_eq!(v["runtime"], "vllm");
        assert_eq!(v["auto_tune"], true);
        let q: Profile = serde_json::from_value(v).unwrap();
        assert_eq!(q.id, p.id);
        let r: Profile = serde_json::from_value(json!({"id": null, "name": "b", "auto_tune": false, "port": 9000})).unwrap();
        assert_eq!(r.spec.runtime, "strata");
        assert!(!r.spec.auto_tune);
        assert_eq!(r.spec.port, Some(9000));
        assert_eq!(r.id.len(), 8);
    }

    #[test]
    fn port_checks() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(!port_free(port));
        drop(l);
        assert!(port_free(port));
    }
}
