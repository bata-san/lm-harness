//! Latest-model catalog with hardware-aware one-click install/uninstall.
//!
//! Routes: `GET /api/catalog`, `GET /api/catalog/plan[/{repo}]`, `POST /api/catalog/install`,
//! `POST /api/catalog/uninstall`.
//!
//! * Repos come from the Hugging Face API (`hf.rs`), cached for a few minutes; when the network
//!   fails the stale cache is served instead.
//! * Every GGUF quantisation is classified by how well it fits this machine (VRAM minus the
//!   current mode's reserve, plus RAM): [`Fit`].
//! * Installs queue a download and may record a *pending install* (persisted in
//!   `HARNESS_HOME/catalog_pending.json`); [`run`] registers the profile / launches the server
//!   once the download completes.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path as UrlPath, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, StreamExt};
use parking_lot::Mutex;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{read_json_file, write_json_file, Settings, HARNESS_HOME};
use crate::downloads::{NewFile, Status as DlStatus};
use crate::error::{ApiError, ApiResult};
use crate::hf::{self, HfFile, HfRepo};
use crate::instances::{LaunchSpec, Profile};
use crate::models;
use crate::state::AppState;
use crate::types::GpuInfo;

const LIST_TTL: Duration = Duration::from_secs(5 * 60);
const TREE_TTL: Duration = Duration::from_secs(10 * 60);
const TREE_TIMEOUT: Duration = Duration::from_secs(25);
const TREE_CONCURRENCY: usize = 8;
const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
/// KV cache + compute buffers + runtime, on top of the weights.
const OVERHEAD: u64 = GIB + GIB / 2;
/// Pending installs whose download never finishes are forgotten after this long.
const PENDING_MAX_AGE_SECS: f64 = 7.0 * 24.0 * 3600.0;
const PENDING_POLL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// Fit classification
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    Gpu,
    MoeOffload,
    Partial,
    Cpu,
    TooBig,
}

impl Fit {
    fn rank(self) -> u8 {
        match self {
            Fit::Gpu => 0,
            Fit::MoeOffload => 1,
            Fit::Partial => 2,
            Fit::Cpu => 3,
            Fit::TooBig => 4,
        }
    }
}

/// Memory this machine can give to a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hw {
    /// VRAM (all GPUs) minus the mode's desktop reserve.
    pub vram_budget: u64,
    pub vram_total: u64,
    pub ram_total: u64,
}

impl Hw {
    pub fn new(gpus: &[GpuInfo], vram_reserve_mb: u64, ram_total: u64) -> Self {
        let total: f64 = gpus.iter().map(|g| g.memory_total_mb.max(0.0)).sum();
        let vram_total = (total * MIB as f64) as u64;
        Self { vram_budget: vram_total.saturating_sub(vram_reserve_mb.saturating_mul(MIB)), vram_total, ram_total }
    }

    fn ram_budget(&self) -> u64 {
        self.ram_total / 20 * 17
    }
}

/// How well a model of `size` bytes runs on `hw`. `moe`: mixture-of-experts (the expert FFNs can
/// live in system RAM while attention and the KV cache stay on the GPU).
pub fn classify(size: u64, moe: bool, hw: &Hw) -> Fit {
    let need = size.saturating_add(OVERHEAD);
    if hw.vram_budget >= need {
        return Fit::Gpu;
    }
    if need > hw.vram_budget.saturating_add(hw.ram_budget()) {
        return Fit::TooBig;
    }
    if hw.vram_budget == 0 {
        return Fit::Cpu;
    }
    if moe && hw.vram_budget >= size / 5 + GIB {
        return Fit::MoeOffload;
    }
    if hw.vram_budget.saturating_mul(100) >= size.saturating_mul(35) {
        Fit::Partial
    } else {
        Fit::Cpu
    }
}

static MOE_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(\bmoe\b|[-_.]moe|\d+x\d+b|[-_]a\d+(\.\d+)?b\b|gpt-oss|mixtral|deepseek-(v2|v3|r1)|minimax-m|glm-4\.\d|llama-4|ernie-4\.5-\d+b-a)").expect("moe regex")
});

/// Heuristic: is this repo a mixture-of-experts model (by name / tags)?
pub fn is_moe(repo: &str, tags: &[String]) -> bool {
    MOE_NAME.is_match(repo) || tags.iter().any(|t| t.eq_ignore_ascii_case("moe") || t.to_ascii_lowercase().contains("mixture-of-experts"))
}

// ---------------------------------------------------------------------------------------------
// Quantisation options
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlanFile {
    pub path: String,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QuantOption {
    pub quant: String,
    pub size: u64,
    pub files: Vec<PlanFile>,
    pub fit: Fit,
    pub recommended: bool,
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct CatalogPlan {
    pub repo: String,
    pub recommended_quant: Option<String>,
    pub options: Vec<QuantOption>,
}

#[derive(Debug, Serialize)]
pub struct Best {
    pub quant: String,
    pub size: u64,
    pub fit: Fit,
    pub files: usize,
}

#[derive(Debug, Serialize)]
pub struct CatalogItem {
    pub id: String,
    pub downloads: u64,
    pub likes: u64,
    pub updated: Option<String>,
    pub created_at: Option<String>,
    pub pipeline_tag: Option<String>,
    pub tags: Vec<String>,
    pub installed: bool,
    pub best: Option<Best>,
}

static SHARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(.*)-(\d{5})-of-(\d{5})\.gguf$").expect("shard regex"));

/// Preferred quantisations, best default first (balance of quality, size and speed).
const PREFERRED: &[&str] = &[
    "Q4_K_M", "Q4_K_XL", "Q5_K_M", "Q4_K_S", "Q5_K_S", "Q6_K", "Q8_0", "IQ4_XS", "Q4_0", "Q4_1", "MXFP4", "Q3_K_XL", "Q3_K_M", "Q3_K_L", "Q3_K_S", "IQ3_M", "IQ3_XS", "IQ3_XXS",
    "Q2_K", "IQ2_M", "IQ2_S", "IQ2_XS", "BF16", "F16",
];

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_model_gguf(path: &str) -> bool {
    let n = file_name(path).to_ascii_lowercase();
    n.ends_with(".gguf") && !n.contains("mmproj") && !n.contains("imatrix")
}

fn quant_label(path: &str) -> String {
    if let Some(q) = hf::quant_of(path) {
        return q;
    }
    // Some repos keep one directory per quantisation: `Q4_K_M/model-00001-of-00002.gguf`.
    if let Some((dir, _)) = path.rsplit_once('/') {
        if let Some(q) = hf::quant_of(dir) {
            return q;
        }
    }
    let n = file_name(path);
    let stem = SHARD.captures(n).map(|c| c[1].to_string()).unwrap_or_else(|| n.trim_end_matches(".gguf").to_string());
    stem
}

fn gib(b: u64) -> f64 {
    b as f64 / GIB as f64
}

fn note_for(fit: Fit, size: u64, hw: &Hw) -> String {
    let need = gib(size.saturating_add(OVERHEAD));
    match fit {
        Fit::Gpu => format!("必要メモリ 約{need:.1} GiB / 使用可能 VRAM {:.1} GiB", gib(hw.vram_budget)),
        Fit::MoeOffload => format!("約{need:.1} GiB: エキスパート層を CPU メモリへ退避 (VRAM {:.1} GiB)", gib(hw.vram_budget)),
        Fit::Partial => format!("約{need:.1} GiB: VRAM {:.1} GiB に載らない層は CPU で処理", gib(hw.vram_budget)),
        Fit::Cpu => format!("約{need:.1} GiB: VRAM {:.1} GiB ではほぼ載らず CPU 推論", gib(hw.vram_budget)),
        Fit::TooBig => format!("約{need:.1} GiB: VRAM {:.1} + RAM {:.1} GiB を超えます", gib(hw.vram_budget), gib(hw.ram_budget())),
    }
}

/// One option per quantisation found in the repo's file list (split sets are summed; incomplete
/// split sets and mmproj/imatrix files are ignored). Largest first.
pub fn build_options(files: &[HfFile], moe: bool, hw: &Hw) -> Vec<QuantOption> {
    // unit key -> (declared shard count, files)
    let mut units: BTreeMap<String, (usize, Vec<&HfFile>)> = BTreeMap::new();
    for f in files.iter().filter(|f| is_model_gguf(&f.path)) {
        let (key, total) = match SHARD.captures(&f.path) {
            Some(c) => (format!("{}.gguf", &c[1]), c[3].parse::<usize>().unwrap_or(1)),
            None => (f.path.clone(), 1),
        };
        units.entry(key).or_insert((total, Vec::new())).1.push(f);
    }
    let mut by_quant: BTreeMap<String, Vec<&HfFile>> = BTreeMap::new();
    for (_, (total, mut parts)) in units {
        parts.sort_by(|a, b| a.path.cmp(&b.path));
        parts.dedup_by(|a, b| a.path == b.path);
        if parts.len() != total {
            continue;
        }
        by_quant.entry(quant_label(&parts[0].path)).or_insert(parts);
    }
    let mut options: Vec<QuantOption> = by_quant
        .into_iter()
        .map(|(quant, parts)| {
            let size: u64 = parts.iter().map(|f| f.size).sum();
            let fit = classify(size, moe, hw);
            QuantOption {
                quant,
                size,
                files: parts.iter().map(|f| PlanFile { path: f.path.clone(), size: f.size, sha256: f.sha256.clone() }).collect(),
                fit,
                recommended: false,
                note: note_for(fit, size, hw),
            }
        })
        .collect();
    options.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.quant.cmp(&b.quant)));
    if let Some(i) = pick_recommended(&options) {
        options[i].recommended = true;
    }
    options
}

/// Best-fitting tier first; inside it the most balanced quantisation (`PREFERRED`), else the largest.
pub fn pick_recommended(options: &[QuantOption]) -> Option<usize> {
    let best = options.iter().map(|o| o.fit.rank()).min()?;
    if best >= Fit::TooBig.rank() {
        return None;
    }
    let tier: Vec<usize> = (0..options.len()).filter(|i| options[*i].fit.rank() == best).collect();
    for p in PREFERRED {
        if let Some(i) = tier.iter().copied().find(|i| options[*i].quant == *p) {
            return Some(i);
        }
    }
    tier.into_iter().max_by_key(|i| options[*i].size)
}

fn plan_from(repo: &str, files: &[HfFile], moe: bool, hw: &Hw) -> CatalogPlan {
    let options = build_options(files, moe, hw);
    let recommended_quant = options.iter().find(|o| o.recommended).map(|o| o.quant.clone());
    CatalogPlan { repo: repo.to_string(), recommended_quant, options }
}

// ---------------------------------------------------------------------------------------------
// Cache (fresh for a TTL, stale copy kept for network failures)
// ---------------------------------------------------------------------------------------------

struct TtlCache<T> {
    ttl: Duration,
    map: Mutex<HashMap<String, (Instant, Arc<T>)>>,
}

impl<T> TtlCache<T> {
    fn new(ttl: Duration) -> Self {
        Self { ttl, map: Mutex::new(HashMap::new()) }
    }

    fn fresh(&self, key: &str) -> Option<Arc<T>> {
        self.map.lock().get(key).filter(|(at, _)| at.elapsed() < self.ttl).map(|(_, v)| v.clone())
    }

    fn stale(&self, key: &str) -> Option<Arc<T>> {
        self.map.lock().get(key).map(|(_, v)| v.clone())
    }

    fn put(&self, key: String, value: Arc<T>) {
        let mut m = self.map.lock();
        if m.len() >= 256 {
            m.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(3600));
        }
        m.insert(key, (Instant::now(), value));
    }
}

/// Fresh cache hit, else fetch; if the fetch fails, serve the stale entry when there is one.
async fn cached<T, F, Fut>(cache: &TtlCache<T>, key: &str, fetch: F) -> anyhow::Result<Arc<T>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    if let Some(v) = cache.fresh(key) {
        return Ok(v);
    }
    match fetch().await {
        Ok(v) => {
            let v = Arc::new(v);
            cache.put(key.to_string(), v.clone());
            Ok(v)
        }
        Err(e) => match cache.stale(key) {
            Some(v) => {
                tracing::warn!("catalog: refresh of {key} failed, serving stale cache: {e:#}");
                Ok(v)
            }
            None => Err(e),
        },
    }
}

static LISTS: LazyLock<TtlCache<Vec<HfRepo>>> = LazyLock::new(|| TtlCache::new(LIST_TTL));
static TREES: LazyLock<TtlCache<Vec<HfFile>>> = LazyLock::new(|| TtlCache::new(TREE_TTL));

async fn repos_cached(settings: &Settings, sort: &str, q: &str, limit: u32) -> anyhow::Result<Arc<Vec<HfRepo>>> {
    let key = format!("{sort}\u{1}{}\u{1}{limit}", q.to_lowercase());
    cached(&LISTS, &key, || hf::search_sorted(settings, q, sort, true, limit)).await
}

async fn files_cached(settings: &Settings, repo: &str) -> anyhow::Result<Arc<Vec<HfFile>>> {
    cached(&TREES, repo, || hf::files(settings, repo, "main")).await
}

// ---------------------------------------------------------------------------------------------
// Hardware
// ---------------------------------------------------------------------------------------------

async fn hardware(state: &AppState) -> Hw {
    let telemetry = state.telemetry.clone();
    let gpus = tokio::task::spawn_blocking(move || telemetry.gpus()).await.unwrap_or_default();
    let ram = tokio::task::spawn_blocking(|| {
        let mut s = sysinfo::System::new();
        s.refresh_memory();
        s.total_memory()
    })
    .await
    .unwrap_or(0);
    Hw::new(&gpus, state.profile().vram_reserve_mb, ram)
}

// ---------------------------------------------------------------------------------------------
// Pending installs
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub download_id: String,
    pub repo: String,
    pub quant: String,
    /// Model id under models_dir (first shard).
    pub model_id: String,
    pub launch: bool,
    pub profile: bool,
    pub created_at: f64,
}

pub struct PendingStore {
    path: PathBuf,
    items: Mutex<Vec<Pending>>,
}

impl PendingStore {
    pub fn load(path: PathBuf) -> Self {
        let items = read_json_file::<Vec<Pending>>(&path).unwrap_or_default();
        Self { path, items: Mutex::new(items) }
    }

    fn persist(&self, items: &[Pending]) {
        if let Err(e) = write_json_file(&self.path, items) {
            tracing::warn!("catalog: cannot persist pending installs: {e:#}");
        }
    }

    pub fn list(&self) -> Vec<Pending> {
        self.items.lock().clone()
    }

    pub fn add(&self, p: Pending) {
        let mut g = self.items.lock();
        g.retain(|x| x.download_id != p.download_id);
        g.push(p);
        self.persist(&g);
    }

    pub fn remove(&self, download_id: &str) {
        let mut g = self.items.lock();
        let before = g.len();
        g.retain(|x| x.download_id != download_id);
        if g.len() != before {
            self.persist(&g);
        }
    }

    pub fn remove_where(&self, f: impl Fn(&Pending) -> bool) {
        let mut g = self.items.lock();
        let before = g.len();
        g.retain(|x| !f(x));
        if g.len() != before {
            self.persist(&g);
        }
    }
}

static PENDING: LazyLock<PendingStore> = LazyLock::new(|| PendingStore::load(HARNESS_HOME.join("catalog_pending.json")));

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Wait,
    Finish,
    Drop,
}

/// What to do with a pending install given its download's status (`None` = download is gone).
fn decide(status: Option<DlStatus>, age_secs: f64) -> Decision {
    match status {
        Some(DlStatus::Completed) => Decision::Finish,
        None | Some(DlStatus::Cancelled) => Decision::Drop,
        _ if age_secs > PENDING_MAX_AGE_SECS => Decision::Drop,
        _ => Decision::Wait,
    }
}

/// `org/Repo-GGUF/Model-Q4_K_M.gguf` -> `Model-Q4_K_M`
fn display_name(p: &Pending) -> String {
    let stem = file_name(&p.model_id);
    let stem = SHARD.captures(stem).map(|c| c[1].to_string()).unwrap_or_else(|| stem.trim_end_matches(".gguf").to_string());
    stem
}

async fn finish_install(state: &AppState, p: &Pending) {
    let dir = state.settings.get().models_dir();
    match models::resolve(&dir, &p.model_id) {
        Ok(path) if path.is_file() => {}
        _ => {
            tracing::warn!("catalog: {} finished downloading but {} is missing", p.repo, p.model_id);
            return;
        }
    }
    let spec = LaunchSpec { name: display_name(p), runtime: "llama.cpp".into(), model: p.model_id.clone(), ..Default::default() };
    if p.profile {
        let exists = state.instances.profiles().iter().any(|x| x.spec.runtime == "llama.cpp" && x.spec.model == p.model_id);
        if !exists {
            let profile = Profile { id: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(), spec: spec.clone() };
            if let Err(e) = state.instances.save_profile(profile) {
                tracing::warn!("catalog: saving profile for {} failed: {e:#}", p.model_id);
            }
        }
    }
    if p.launch {
        let running = state.instances.alive_instances().iter().any(|i| i.model == p.model_id);
        if !running {
            if let Err(e) = state.instances.launch(state, spec).await {
                tracing::warn!("catalog: launching {} failed: {e:#}", p.model_id);
            }
        }
    }
}

/// Background loop: finishes pending installs (register profile / launch when the download completes).
pub async fn run(state: AppState) {
    loop {
        tokio::time::sleep(PENDING_POLL).await;
        for p in PENDING.list() {
            let status = state.downloads.get(&p.download_id).map(|t| t.status);
            match decide(status, now_secs() - p.created_at) {
                Decision::Wait => {}
                Decision::Drop => PENDING.remove(&p.download_id),
                Decision::Finish => {
                    // Forget it first so a slow launch is never repeated.
                    PENDING.remove(&p.download_id);
                    finish_install(&state, &p).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Routes: list / plan
// ---------------------------------------------------------------------------------------------

fn default_sort() -> String {
    "trending".into()
}
fn default_limit() -> u32 {
    24
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default, alias = "query")]
    q: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

/// Map the UI sort name to the Hub's sort key.
fn hub_sort(sort: &str) -> Option<&'static str> {
    match sort {
        "trending" => Some("trendingScore"),
        "recent" => Some("createdAt"),
        "downloads" => Some("downloads"),
        _ => None,
    }
}

fn blocking_err(e: tokio::task::JoinError) -> ApiError {
    ApiError::internal(e.to_string())
}

fn gateway(e: anyhow::Error) -> ApiError {
    ApiError::bad_gateway(format!("{e:#}"))
}

async fn list_catalog(State(st): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Vec<CatalogItem>> {
    let sort = hub_sort(&q.sort).ok_or_else(|| ApiError::bad_request("sort は trending / recent / downloads のいずれかです"))?;
    let settings = st.settings.get();
    let limit = q.limit.clamp(1, 60);
    let repos = repos_cached(&settings, sort, q.q.trim(), limit).await.map_err(gateway)?;
    let hw = hardware(&st).await;

    let dir = settings.models_dir();
    let local: Vec<String> = tokio::task::spawn_blocking(move || models::scan(&dir).into_iter().map(|m| m.id).collect()).await.map_err(blocking_err)?;

    // Trees of every repo (cached; one Hub call each, a few at a time).
    let rows: Vec<(HfRepo, Option<Arc<Vec<HfFile>>>)> = stream::iter(repos.iter().cloned())
        .map(|r| {
            let s = settings.clone();
            async move {
                let files = match tokio::time::timeout(TREE_TIMEOUT, files_cached(&s, &r.id)).await {
                    Ok(Ok(f)) => Some(f),
                    _ => None,
                };
                (r, files)
            }
        })
        .buffered(TREE_CONCURRENCY)
        .collect()
        .await;

    let items = rows
        .into_iter()
        .map(|(r, files)| {
            let moe = is_moe(&r.id, &r.tags);
            let best = files.and_then(|f| {
                let options = build_options(&f, moe, &hw);
                options.into_iter().find(|o| o.recommended).map(|o| Best { quant: o.quant, size: o.size, fit: o.fit, files: o.files.len() })
            });
            let prefix = format!("{}/", r.id);
            let installed = local.iter().any(|id| id.starts_with(&prefix) && id.to_ascii_lowercase().ends_with(".gguf"));
            CatalogItem { id: r.id, downloads: r.downloads, likes: r.likes, updated: r.updated, created_at: r.created_at, pipeline_tag: r.pipeline_tag, tags: r.tags, installed, best }
        })
        .collect();
    Ok(Json(items))
}

/// `owner/name` with the characters Hugging Face allows.
fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else { return false };
    let ok = |s: &str| !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    ok(a) && ok(b)
}

async fn plan_for(st: &AppState, repo: &str) -> Result<CatalogPlan, ApiError> {
    let repo = repo.trim().trim_matches('/');
    if !valid_repo(repo) {
        return Err(ApiError::bad_request("repo は owner/name 形式で指定してください"));
    }
    let files = files_cached(&st.settings.get(), repo).await.map_err(gateway)?;
    let hw = hardware(st).await;
    Ok(plan_from(repo, &files, is_moe(repo, &[]), &hw))
}

#[derive(Deserialize)]
struct PlanQuery {
    repo: Option<String>,
}

async fn plan_by_query(State(st): State<AppState>, Query(q): Query<PlanQuery>) -> ApiResult<CatalogPlan> {
    let repo = q.repo.ok_or_else(|| ApiError::bad_request("repo が必要です"))?;
    Ok(Json(plan_for(&st, &repo).await?))
}

async fn plan_by_path(State(st): State<AppState>, UrlPath(repo): UrlPath<String>) -> ApiResult<CatalogPlan> {
    Ok(Json(plan_for(&st, &repo).await?))
}

// ---------------------------------------------------------------------------------------------
// Routes: install
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct InstallReq {
    repo: String,
    #[serde(default)]
    quant: Option<String>,
    /// Launch the server once the download completes.
    #[serde(default)]
    launch: bool,
    /// Save a launch profile once the download completes.
    #[serde(default)]
    profile: bool,
}

#[derive(Serialize)]
struct InstallResp {
    download_id: String,
    quant: String,
    files: usize,
}

/// Pick the requested quantisation (case-insensitive) or the recommended one.
fn choose_option<'a>(plan: &'a CatalogPlan, quant: Option<&str>) -> Result<&'a QuantOption, ApiError> {
    let opt = match quant.map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => plan.options.iter().find(|o| o.quant.eq_ignore_ascii_case(q)).ok_or_else(|| ApiError::not_found(format!("量子化 {q} が見つかりません")))?,
        None => plan.options.iter().find(|o| o.recommended).ok_or_else(|| ApiError::bad_request("このマシンで動作する量子化がありません"))?,
    };
    if opt.fit == Fit::TooBig {
        return Err(ApiError::bad_request(format!("{} はこのマシンのメモリに収まりません", opt.quant)));
    }
    Ok(opt)
}

async fn install(State(st): State<AppState>, Json(req): Json<InstallReq>) -> ApiResult<InstallResp> {
    let plan = plan_for(&st, &req.repo).await?;
    let opt = choose_option(&plan, req.quant.as_deref())?;
    let files: Vec<NewFile> = opt.files.iter().map(|f| NewFile { path: f.path.clone(), size: Some(f.size), sha256: f.sha256.clone() }).collect();
    let first = opt.files.first().ok_or_else(|| ApiError::internal("empty file list"))?;
    let task = st.downloads.add_hf(&plan.repo, files, None).map_err(ApiError::bad_request)?;
    if req.launch || req.profile {
        PENDING.add(Pending {
            download_id: task.id.clone(),
            repo: plan.repo.clone(),
            quant: opt.quant.clone(),
            model_id: format!("{}/{}", plan.repo, first.path),
            launch: req.launch,
            profile: req.profile,
            created_at: now_secs(),
        });
    }
    Ok(Json(InstallResp { download_id: task.id, quant: opt.quant.clone(), files: opt.files.len() }))
}

// ---------------------------------------------------------------------------------------------
// Routes: uninstall
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct UninstallReq {
    /// Model id (path relative to models_dir).
    model: String,
}

#[derive(Serialize)]
struct UninstallResp {
    stopped: Vec<String>,
    profiles_removed: usize,
    freed_bytes: u64,
}

fn split_key(name: &str) -> Option<(String, String)> {
    SHARD.captures(name).map(|c| (c[1].to_string(), c[3].to_string()))
}

/// Does `candidate` (a downloaded file path) belong to the model at `target`: the file itself, a
/// file inside the model directory, or a sibling shard of the same split set?
fn shares_model(target: &Path, candidate: &Path) -> bool {
    if candidate == target || candidate.starts_with(target) {
        return true;
    }
    if candidate.parent() != target.parent() {
        return false;
    }
    let key = |p: &Path| p.file_name().and_then(|n| n.to_str()).and_then(split_key);
    matches!((key(target), key(candidate)), (Some(a), Some(b)) if a == b)
}

/// Does a profile / instance model id refer to the model id being removed?
fn model_matches(used: &str, id: &str) -> bool {
    used == id || used.strip_prefix(id).is_some_and(|rest| rest.starts_with('/'))
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(read) = std::fs::read_dir(dir) else { return 0 };
    read.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Bytes a deletion of `path` frees (whole split set for a shard, whole tree for a directory).
fn target_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if meta.is_dir() {
        return dir_size(path);
    }
    let mut total = meta.len();
    if let (Some(key), Some(dir)) = (path.file_name().and_then(|n| n.to_str()).and_then(split_key), path.parent()) {
        if let Ok(read) = std::fs::read_dir(dir) {
            for e in read.flatten() {
                let p = e.path();
                let sibling = p != path && e.file_type().is_ok_and(|t| t.is_file()) && e.file_name().to_str().and_then(split_key).as_ref() == Some(&key);
                if sibling {
                    total += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    total
}

/// Resolve `id` strictly inside `models_dir` (no traversal, no symlink escape) and measure it.
fn plan_uninstall(models_dir: &Path, id: &str) -> Result<(PathBuf, u64), ApiError> {
    let path = models::resolve(models_dir, id).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    if std::fs::symlink_metadata(&path).is_err() {
        return Err(ApiError::not_found("モデルが見つかりません"));
    }
    let size = target_size(&path);
    Ok((path, size))
}

async fn uninstall(State(st): State<AppState>, Json(req): Json<UninstallReq>) -> ApiResult<UninstallResp> {
    let id = req.model.trim().to_string();
    let models_dir = st.settings.get().models_dir();

    let (path, freed_bytes) = {
        let (dir, id) = (models_dir.clone(), id.clone());
        tokio::task::spawn_blocking(move || plan_uninstall(&dir, &id)).await.map_err(blocking_err)??
    };

    // Servers still using the files must go first.
    let mut stopped = Vec::new();
    for inst in st.instances.alive_instances() {
        if model_matches(&inst.model, &id) {
            inst.proc.stop().await;
            stopped.push(inst.id().to_string());
        }
    }

    // Downloads writing into (or finished for) this model.
    for t in st.downloads.list() {
        if t.kind != crate::downloads::Kind::Hf || matches!(t.status, DlStatus::Cancelled) {
            continue;
        }
        let base = Path::new(&t.dest);
        let related = t.files.iter().any(|f| shares_model(&path, &base.join(&f.path)));
        if related && t.status != DlStatus::Completed {
            st.downloads.cancel(&t.id);
        }
    }
    PENDING.remove_where(|p| model_matches(&p.model_id, &id));

    let (dir, del_id) = (models_dir, id.clone());
    tokio::task::spawn_blocking(move || models::delete(&dir, &del_id)).await.map_err(blocking_err)??;

    let mut profiles_removed = 0;
    for p in st.instances.profiles() {
        if model_matches(&p.spec.model, &id) && st.instances.delete_profile(&p.id).is_ok() {
            profiles_removed += 1;
        }
    }
    Ok(Json(UninstallResp { stopped, profiles_removed, freed_bytes }))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/catalog", get(list_catalog))
        .route("/api/catalog/plan", get(plan_by_query))
        .route("/api/catalog/plan/{*repo}", get(plan_by_path))
        .route("/api/catalog/install", post(install))
        .route("/api/catalog/uninstall", post(uninstall))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn gpu(total_mb: f64) -> GpuInfo {
        GpuInfo { memory_total_mb: total_mb, ..Default::default() }
    }

    /// 24 GiB GPU, 64 GiB RAM, 512 MiB reserve (full mode).
    fn hw24() -> Hw {
        Hw::new(&[gpu(24.0 * 1024.0)], 512, 64 * GIB)
    }

    fn file(path: &str, gib_size: f64) -> HfFile {
        HfFile { path: path.into(), size: (gib_size * GIB as f64) as u64, quant: hf::quant_of(path), split: hf::is_split(path), sha256: None }
    }

    #[test]
    fn classification_tiers() {
        let hw = hw24();
        let g = |x: f64| (x * GIB as f64) as u64;
        assert_eq!(classify(g(4.4), false, &hw), Fit::Gpu);
        assert_eq!(classify(g(18.0), true, &hw), Fit::Gpu);
        assert_eq!(classify(g(63.0), true, &hw), Fit::MoeOffload);
        assert_eq!(classify(g(42.0), false, &hw), Fit::Partial);
        assert_eq!(classify(g(75.0), false, &hw), Fit::Cpu);
        assert_eq!(classify(g(120.0), false, &hw), Fit::TooBig);
        assert_eq!(classify(g(120.0), true, &hw), Fit::TooBig);
    }

    #[test]
    fn shared_mode_reserve_shrinks_the_gpu_tier() {
        let g = |x: f64| (x * GIB as f64) as u64;
        let full = Hw::new(&[gpu(24.0 * 1024.0)], 512, 64 * GIB);
        let shared = Hw::new(&[gpu(24.0 * 1024.0)], 4096, 64 * GIB);
        assert_eq!(classify(g(21.0), false, &full), Fit::Gpu);
        assert_ne!(classify(g(21.0), false, &shared), Fit::Gpu);
        assert_eq!(shared.vram_budget, 20 * GIB);
    }

    #[test]
    fn no_gpu_is_cpu_or_too_big() {
        let hw = Hw::new(&[], 512, 32 * GIB);
        let g = |x: f64| (x * GIB as f64) as u64;
        assert_eq!(hw.vram_budget, 0);
        assert_eq!(classify(g(4.0), false, &hw), Fit::Cpu);
        assert_eq!(classify(g(4.0), true, &hw), Fit::Cpu);
        assert_eq!(classify(g(40.0), false, &hw), Fit::TooBig);
    }

    #[test]
    fn moe_detection() {
        for yes in ["unsloth/Qwen3-30B-A3B-GGUF", "ggml-org/gpt-oss-120b-GGUF", "TheBloke/Mixtral-8x7B-v0.1-GGUF", "x/some-MoE-model"] {
            assert!(is_moe(yes, &[]), "{yes}");
        }
        assert!(is_moe("x/y", &["moe".to_string()]));
        assert!(!is_moe("bartowski/Llama-3.1-8B-Instruct-GGUF", &[]));
        assert!(!is_moe("bartowski/Qwen2.5-72B-Instruct-GGUF", &[]));
    }

    #[test]
    fn options_group_splits_and_pick_recommendation() {
        let files = vec![
            file("M-Q8_0.gguf", 8.0),
            file("M-Q4_K_M.gguf", 4.5),
            file("M-Q2_K.gguf", 2.8),
            file("M-F16-00001-of-00002.gguf", 8.0),
            file("M-F16-00002-of-00002.gguf", 8.0),
            file("M-Q5_K_M-00001-of-00003.gguf", 2.0), // incomplete split set -> ignored
            file("mmproj-M-F16.gguf", 1.0),
            file("README.md", 0.0),
        ];
        let o = build_options(&files, false, &hw24());
        let quants: Vec<&str> = o.iter().map(|x| x.quant.as_str()).collect();
        assert_eq!(quants, vec!["F16", "Q8_0", "Q4_K_M", "Q2_K"]);
        assert_eq!(o[0].files.len(), 2);
        assert_eq!(o[0].size, 16 * GIB);
        assert!(o.iter().all(|x| x.fit == Fit::Gpu));
        let rec: Vec<&str> = o.iter().filter(|x| x.recommended).map(|x| x.quant.as_str()).collect();
        assert_eq!(rec, vec!["Q4_K_M"]);
    }

    #[test]
    fn recommendation_prefers_the_best_tier_over_quant_preference() {
        // Q4_K_M only fits partially on a small GPU, Q2_K fits fully.
        let hw = Hw::new(&[gpu(8.0 * 1024.0)], 512, 32 * GIB);
        let files = vec![file("M-Q4_K_M.gguf", 9.0), file("M-Q2_K.gguf", 5.0)];
        let o = build_options(&files, false, &hw);
        let q4 = o.iter().find(|x| x.quant == "Q4_K_M").unwrap();
        let q2 = o.iter().find(|x| x.quant == "Q2_K").unwrap();
        assert_eq!(q4.fit, Fit::Partial);
        assert_eq!(q2.fit, Fit::Gpu);
        assert!(q2.recommended && !q4.recommended);
    }

    #[test]
    fn nothing_recommended_when_everything_is_too_big() {
        let hw = Hw::new(&[gpu(8.0 * 1024.0)], 512, 16 * GIB);
        let o = build_options(&[file("M-Q4_K_M.gguf", 60.0)], false, &hw);
        assert_eq!(o[0].fit, Fit::TooBig);
        assert!(o.iter().all(|x| !x.recommended));
        let plan = plan_from("a/b", &[file("M-Q4_K_M.gguf", 60.0)], false, &hw);
        assert_eq!(plan.recommended_quant, None);
        assert!(choose_option(&plan, None).is_err());
        assert!(choose_option(&plan, Some("q4_k_m")).is_err());
    }

    #[test]
    fn quant_from_directory_and_plain_names() {
        let files = vec![file("Q6_K/model-00001-of-00002.gguf", 5.0), file("Q6_K/model-00002-of-00002.gguf", 5.0), file("plain-model.gguf", 3.0)];
        let o = build_options(&files, false, &hw24());
        let quants: Vec<&str> = o.iter().map(|x| x.quant.as_str()).collect();
        assert_eq!(quants, vec!["Q6_K", "plain-model"]);
        assert_eq!(o[0].files[0].path, "Q6_K/model-00001-of-00002.gguf");
    }

    #[test]
    fn plan_json_shape_matches_frontend() {
        let plan = plan_from("a/b", &[file("M-Q4_K_M.gguf", 4.0)], false, &hw24());
        let v = serde_json::to_value(&plan).unwrap();
        assert_eq!(v["repo"], "a/b");
        assert_eq!(v["recommended_quant"], "Q4_K_M");
        let o = &v["options"][0];
        assert_eq!(o["fit"], "gpu");
        assert_eq!(o["recommended"], true);
        assert!(o["files"][0]["path"].is_string() && o["files"][0]["size"].is_u64());
        assert!(o["note"].is_string());
        assert_eq!(serde_json::to_value(Fit::MoeOffload).unwrap(), "moe_offload");
        assert_eq!(serde_json::to_value(Fit::TooBig).unwrap(), "too_big");
    }

    #[test]
    fn repo_validation_and_sort_mapping() {
        assert!(valid_repo("unsloth/Qwen3-30B-A3B-GGUF"));
        for bad in ["", "x", "a/b/c", "../x", "a/..", "a/b c", "/a/b", "a//b"] {
            assert!(!valid_repo(bad), "{bad:?}");
        }
        assert_eq!(hub_sort("trending"), Some("trendingScore"));
        assert_eq!(hub_sort("recent"), Some("createdAt"));
        assert_eq!(hub_sort("downloads"), Some("downloads"));
        assert_eq!(hub_sort("evil"), None);
    }

    // ---- cache

    #[tokio::test]
    async fn cache_serves_stale_on_failure_and_fresh_without_refetch() {
        let cache: TtlCache<u32> = TtlCache::new(Duration::from_millis(30));
        let calls = AtomicUsize::new(0);
        let v = cached(&cache, "k", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        })
        .await
        .unwrap();
        assert_eq!(*v, 1);
        // fresh hit: no second fetch
        let v = cached(&cache, "k", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(2)
        })
        .await
        .unwrap();
        assert_eq!((*v, calls.load(Ordering::SeqCst)), (1, 1));
        // expired + network failure -> stale copy
        tokio::time::sleep(Duration::from_millis(50)).await;
        let v = cached(&cache, "k", || async { Err(anyhow::anyhow!("offline")) }).await.unwrap();
        assert_eq!(*v, 1);
        // no stale entry -> the error is surfaced
        assert!(cached(&cache, "other", || async { Err::<u32, _>(anyhow::anyhow!("offline")) }).await.is_err());
    }

    // ---- pending installs

    fn pending(id: &str, model: &str) -> Pending {
        Pending { download_id: id.into(), repo: "o/r".into(), quant: "Q4_K_M".into(), model_id: model.into(), launch: true, profile: true, created_at: 1.0 }
    }

    #[test]
    fn pending_store_persists_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("catalog_pending.json");
        let s = PendingStore::load(path.clone());
        s.add(pending("a", "o/r/m.gguf"));
        s.add(pending("b", "o/r/n.gguf"));
        s.add(pending("a", "o/r/m2.gguf")); // replaces
        let again = PendingStore::load(path.clone());
        assert_eq!(again.list().len(), 2);
        assert_eq!(again.list().iter().find(|p| p.download_id == "a").unwrap().model_id, "o/r/m2.gguf");
        again.remove("a");
        again.remove_where(|p| p.model_id == "o/r/n.gguf");
        assert!(PendingStore::load(path).list().is_empty());
    }

    #[test]
    fn pending_decisions() {
        assert_eq!(decide(Some(DlStatus::Completed), 1.0), Decision::Finish);
        assert_eq!(decide(Some(DlStatus::Downloading), 1.0), Decision::Wait);
        assert_eq!(decide(Some(DlStatus::Queued), 1.0), Decision::Wait);
        assert_eq!(decide(Some(DlStatus::Paused), 1.0), Decision::Wait);
        assert_eq!(decide(Some(DlStatus::Failed), 1.0), Decision::Wait);
        assert_eq!(decide(Some(DlStatus::Cancelled), 1.0), Decision::Drop);
        assert_eq!(decide(None, 1.0), Decision::Drop);
        assert_eq!(decide(Some(DlStatus::Paused), PENDING_MAX_AGE_SECS + 1.0), Decision::Drop);
    }

    #[test]
    fn display_names() {
        assert_eq!(display_name(&pending("a", "o/r/Model-Q4_K_M-00001-of-00003.gguf")), "Model-Q4_K_M");
        assert_eq!(display_name(&pending("a", "o/r/Model-Q4_K_M.gguf")), "Model-Q4_K_M");
    }

    // ---- uninstall safety

    fn touch(p: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    #[test]
    fn uninstall_rejects_paths_outside_models_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        touch(&tmp.path().join("secret.gguf"), b"x");
        touch(&root.join("a/ok.gguf"), b"xyz");

        for bad in ["../secret.gguf", "a/../../secret.gguf", "/etc/passwd", "", ".", "..", "a/../.."] {
            let e = plan_uninstall(&root, bad).unwrap_err();
            assert_eq!(e.0, axum::http::StatusCode::BAD_REQUEST, "{bad:?}");
        }
        assert!(tmp.path().join("secret.gguf").exists());
        let (path, size) = plan_uninstall(&root, "a/ok.gguf").unwrap();
        assert!(path.starts_with(root.canonicalize().unwrap()));
        assert_eq!(size, 3);
        assert_eq!(plan_uninstall(&root, "a/missing.gguf").unwrap_err().0, axum::http::StatusCode::NOT_FOUND);
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_refuses_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        touch(&outside.join("secret.gguf"), b"x");
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(plan_uninstall(&root, "link/secret.gguf").is_err());
        assert!(plan_uninstall(&root, "link").is_err());
    }

    #[test]
    fn freed_bytes_cover_split_sets_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("o/r/M-Q4-00001-of-00002.gguf"), b"aaaa");
        touch(&root.join("o/r/M-Q4-00002-of-00002.gguf"), b"bb");
        touch(&root.join("o/r/other.gguf"), b"zzzzzzzz");
        touch(&root.join("hf/Qwen/config.json"), b"{}");
        touch(&root.join("hf/Qwen/w.safetensors"), b"12345");
        assert_eq!(plan_uninstall(root, "o/r/M-Q4-00001-of-00002.gguf").unwrap().1, 6);
        assert_eq!(plan_uninstall(root, "o/r/other.gguf").unwrap().1, 8);
        assert_eq!(plan_uninstall(root, "hf/Qwen").unwrap().1, 7);
    }

    #[test]
    fn related_download_detection() {
        let target = Path::new("/m/o/r/M-Q4-00001-of-00002.gguf");
        assert!(shares_model(target, Path::new("/m/o/r/M-Q4-00001-of-00002.gguf")));
        assert!(shares_model(target, Path::new("/m/o/r/M-Q4-00002-of-00002.gguf")));
        assert!(!shares_model(target, Path::new("/m/o/r/M-Q8-00001-of-00002.gguf")));
        assert!(!shares_model(target, Path::new("/m/o/r/other.gguf")));
        assert!(!shares_model(target, Path::new("/m/o/x/M-Q4-00002-of-00002.gguf")));
        let dir = Path::new("/m/hf/Qwen");
        assert!(shares_model(dir, Path::new("/m/hf/Qwen/w.safetensors")));
        assert!(!shares_model(dir, Path::new("/m/hf/Qwen2/w.safetensors")));
    }

    #[test]
    fn profile_and_instance_matching() {
        assert!(model_matches("o/r/m.gguf", "o/r/m.gguf"));
        assert!(model_matches("hf/Qwen/sub", "hf/Qwen"));
        assert!(!model_matches("hf/Qwen2", "hf/Qwen"));
        assert!(!model_matches("", "hf/Qwen"));
    }
}
