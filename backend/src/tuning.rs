//! llama.cpp / vLLM launch-parameter recommendations, mode-aware.
//! Routes: POST /api/tune
//!
//! The goal is "everything on the GPU if it fits, otherwise push the least useful weights
//! (MoE experts first) to the CPU". Limits come from the active `ModeProfile`: in shared mode
//! less VRAM is available, fewer cores are used and micro-batches are capped so the desktop
//! stays responsive.

use std::collections::HashSet;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use crate::error::{ApiError, ApiResult};
use crate::mode::{Mode, ModeProfile};
use crate::state::AppState;
use crate::types::{GgufInfo, GpuInfo, LocalModel, TuneEstimate, TuneResult};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// CUDA context + compute buffers + fragmentation headroom.
const OVERHEAD: u64 = GIB * 6 / 5;
const DEFAULT_NATIVE_CTX: u64 = 8192;
const MAX_DEFAULT_CTX: u64 = 32768;

fn bytes_per_elem(kv_type: &str) -> f64 {
    match kv_type {
        "f16" => 2.0,
        "q8_0" => 1.0625,
        "q4_0" => 0.5625,
        _ => 2.0,
    }
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB as f64
}

fn mb_to_bytes(mb: f64) -> u64 {
    (mb.max(0.0) * MIB as f64) as u64
}

/// KV cache size for `ctx` tokens per slot, `parallel` slots.
pub fn kv_cache_bytes(info: &GgufInfo, ctx: u64, kv_type: &str, parallel: u32) -> u64 {
    let layers = info.block_count.unwrap_or(0);
    let n_kv = info.head_count_kv.or(info.head_count).unwrap_or(0);
    let k = info.key_length.unwrap_or(0);
    let v = info.value_length.unwrap_or(k);
    // llama-server splits -c across slots, so the cache scales with --parallel.
    let elems = layers as f64 * ctx as f64 * parallel.max(1) as f64 * n_kv as f64 * (k + v) as f64;
    (elems * bytes_per_elem(kv_type)) as u64
}

pub fn recommend(model: &LocalModel, gpus: &[GpuInfo], profile: &ModeProfile, ctx: Option<u64>, kv_type: &str, parallel: u32) -> TuneResult {
    let info = &model.info;
    let parallel = parallel.max(1);
    let reserve = profile.vram_reserve_mb * MIB;

    let free_mb: f64 = gpus.iter().map(|g| g.memory_free_mb).sum();
    let total_mb: f64 = gpus.iter().map(|g| g.memory_total_mb).sum();
    let vram_free = mb_to_bytes(free_mb).saturating_sub(reserve);
    let vram_total = mb_to_bytes(total_mb);

    let size = model.size;
    let layers = info.block_count.unwrap_or(0);
    let moe = info.expert_count.unwrap_or(0) > 0;
    let native_ctx = info.context_length.filter(|c| *c > 0).unwrap_or(DEFAULT_NATIVE_CTX);
    let ctx = ctx.filter(|c| *c > 0).unwrap_or_else(|| native_ctx.min(MAX_DEFAULT_CTX));

    let kv = kv_cache_bytes(info, ctx, kv_type, parallel);
    let budget = vram_free as i128 - OVERHEAD as i128 - kv as i128;
    let has_gpu = !gpus.is_empty();
    let fits = has_gpu && size as i128 <= budget;

    let mut notes: Vec<String> = Vec::new();
    let mut n_cpu_moe = 0u64;
    let ngl: u64 = if !has_gpu {
        notes.push("GPU 未検出のため CPU 推論。".into());
        0
    } else if fits {
        notes.push(format!(
            "モデル {:.1} GiB + KV {:.1} GiB が VRAM 空き {:.1} GiB に収まるため全層 GPU。",
            gib(size),
            gib(kv),
            gib(vram_free)
        ));
        999
    } else if moe && layers > 0 {
        // Keep attention/shared weights on GPU, offload the expert FFNs of the first N layers.
        let overflow = (size as i128 - budget).max(0) as u64;
        let per_layer = size as f64 / layers as f64;
        // ~90% of a MoE layer is expert weights.
        let raw = overflow as f64 / (per_layer * 0.9);
        n_cpu_moe = (raw as u64).saturating_add(1).min(layers);
        notes.push(format!(
            "MoE モデル ({} experts) が VRAM を {:.1} GiB 超過。先頭 {} 層の expert を CPU に置く (--n-cpu-moe)。Attention は GPU のまま。",
            info.expert_count.unwrap_or(0),
            gib(overflow),
            n_cpu_moe
        ));
        999
    } else if layers > 0 {
        let ratio = if size == 0 { 0.0 } else { budget.max(0) as f64 / size as f64 };
        let ngl = ((layers as f64 * ratio) as u64).min(layers);
        notes.push(format!("VRAM 不足のため {ngl}/{layers} 層を GPU にオフロード。残りは CPU (メモリ帯域が律速)。"));
        ngl
    } else {
        999
    };

    let total_layers = if layers == 0 { 999 } else { layers };
    let gpu_resident = has_gpu && ngl >= total_layers && n_cpu_moe == 0;

    // Threads come from the mode's core set, not the whole machine. Fully-offloaded generation
    // is GPU-bound, so extra CPU threads only add synchronisation overhead.
    let (threads, threads_batch) = if gpu_resident {
        (profile.physical_cores.min(4), profile.physical_cores)
    } else {
        (profile.physical_cores, profile.logical_cpus)
    };

    let (batch, ubatch) = {
        let (b, ub) = if gpu_resident { (4096u64, 1024u64) } else { (2048, 512) };
        let ub = ub.min(profile.ubatch_cap as u64);
        (b.max(ub), ub)
    };

    let mut args: Vec<String> = vec![
        "-ngl".to_string(),
        ngl.to_string(),
        "-c".to_string(),
        (ctx * parallel as u64).to_string(),
        "-t".to_string(),
        threads.to_string(),
        "-tb".to_string(),
        threads_batch.to_string(),
    ];
    if n_cpu_moe > 0 {
        args.push("--n-cpu-moe".to_string());
        args.push(n_cpu_moe.to_string());
    }
    if has_gpu {
        args.push("-fa".to_string());
        args.push("on".to_string());
        if kv_type != "f16" {
            args.extend(["-ctk", kv_type, "-ctv", kv_type].map(String::from));
        }
    }
    args.extend(["-np".to_string(), parallel.to_string(), "-b".to_string(), batch.to_string(), "-ub".to_string(), ubatch.to_string()]);
    if info.has_chat_template {
        args.push("--jinja".to_string());
    }
    args.push("--metrics".to_string());
    if let Some(mmproj) = &model.mmproj {
        args.push("--mmproj".to_string());
        args.push(mmproj.clone());
        notes.push("mmproj (ビジョン) を検出したので付与。".to_string());
    }

    if !gpu_resident {
        notes.push(format!(
            "CPU スレッドは物理コア数 {} に合わせる (SMT は生成速度を上げない)。",
            profile.physical_cores
        ));
    }
    if ctx > native_ctx {
        notes.push(format!("注意: ネイティブ context {native_ctx} を超えています (RoPE scaling が必要)。"));
    }
    if profile.mode == Mode::Shared {
        notes.push(format!(
            "共有モード: VRAM {:.1} GiB をデスクトップ用に確保、CPU {} コア ({} スレッド) に制限、マイクロバッチ上限 {}",
            gib(reserve),
            profile.physical_cores,
            profile.logical_cpus,
            profile.ubatch_cap
        ));
    }

    TuneResult {
        args,
        notes,
        estimate: TuneEstimate {
            model_bytes: size,
            kv_bytes: kv,
            vram_free,
            vram_total,
            vram_reserve: reserve,
            fits,
            ctx,
            native_ctx,
        },
    }
}

/// Default args for vLLM under the given mode.
pub fn vllm_args(profile: &ModeProfile, gpus: &[GpuInfo]) -> Vec<String> {
    let total_mb: f64 = gpus.iter().map(|g| g.memory_total_mb).sum();
    let util = if gpus.is_empty() || total_mb <= 0.0 || profile.mode == Mode::Full {
        0.92
    } else {
        ((total_mb - profile.vram_reserve_mb as f64) / total_mb * 0.95).clamp(0.30, 0.92)
    };
    vec![
        "--gpu-memory-utilization".to_string(),
        format!("{util:.2}"),
        "--max-model-len".to_string(),
        "32768".to_string(),
        "--enable-prefix-caching".to_string(),
    ]
}

/// One CLI token group: a flag (or `None` for a bare value with no flag before it) and its value.
#[derive(Clone, Debug, PartialEq)]
struct Arg {
    flag: Option<String>,
    value: Option<String>,
}

impl Arg {
    fn key(&self) -> Option<&str> {
        self.flag.as_deref().map(canonical_flag)
    }
}

/// Map llama.cpp flag aliases onto one canonical spelling.
fn canonical_flag(flag: &str) -> &str {
    match flag {
        "-ngl" | "--n-gpu-layers" | "--gpu-layers" => "-ngl",
        "-c" | "--ctx-size" => "-c",
        "-t" | "--threads" => "-t",
        "-tb" | "--threads-batch" => "-tb",
        "-fa" | "--flash-attn" => "-fa",
        "-ctk" | "--cache-type-k" => "-ctk",
        "-ctv" | "--cache-type-v" => "-ctv",
        "-np" | "--parallel" => "-np",
        "-b" | "--batch-size" => "-b",
        "-ub" | "--ubatch-size" => "-ub",
        "-m" | "--model" => "-m",
        other => other,
    }
}

/// A flag starts with `-` and is not a negative number (`-1`, `-0.5`).
fn is_flag(tok: &str) -> bool {
    tok.len() > 1 && tok.starts_with('-') && tok.parse::<f64>().is_err()
}

fn parse_args(args: &[String]) -> Vec<Arg> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.iter().peekable();
    while let Some(tok) = iter.next() {
        if !is_flag(tok) {
            out.push(Arg { flag: None, value: Some(tok.clone()) });
            continue;
        }
        let (flag, inline) = match tok.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (tok.clone(), None),
        };
        // The following token is this flag's value unless it is another flag.
        let value = inline.or_else(|| iter.next_if(|next| !is_flag(next)).cloned());
        out.push(Arg { flag: Some(flag), value });
    }
    out
}

/// Merge CLI flags: every flag present in `overrides` replaces the same flag in `base`.
///
/// A replaced flag keeps its position in `base` (with the override's spelling and value);
/// override flags that `base` does not have are appended in their order.
#[allow(dead_code)] // consumed by the instances module when launch arguments are assembled
pub fn merge_args(base: &[String], overrides: &[String]) -> Vec<String> {
    let base = parse_args(base);
    let overrides = parse_args(overrides);
    let base_keys: HashSet<&str> = base.iter().filter_map(Arg::key).collect();
    let override_keys: HashSet<&str> = overrides.iter().filter_map(Arg::key).collect();

    let mut out: Vec<Arg> = Vec::with_capacity(base.len() + overrides.len());
    let mut replaced: HashSet<&str> = HashSet::new();
    for arg in &base {
        match arg.key() {
            Some(k) if override_keys.contains(k) => {
                // Emit the override group once, at the first base occurrence of the flag.
                if replaced.insert(k) {
                    out.extend(overrides.iter().filter(|o| o.key() == Some(k)).cloned());
                }
            }
            _ => out.push(arg.clone()),
        }
    }
    for arg in &overrides {
        match arg.key() {
            Some(k) if base_keys.contains(k) => {}
            _ => out.push(arg.clone()),
        }
    }

    out.into_iter().flat_map(|a| a.flag.into_iter().chain(a.value)).collect()
}

#[derive(Deserialize)]
struct TuneReq {
    model: String,
    #[serde(default)]
    ctx: Option<u64>,
    #[serde(default = "default_kv_type")]
    kv_type: String,
    #[serde(default = "default_parallel")]
    parallel: u32,
}

fn default_kv_type() -> String {
    "q8_0".to_string()
}

fn default_parallel() -> u32 {
    1
}

async fn tune(State(state): State<AppState>, Json(req): Json<TuneReq>) -> ApiResult<TuneResult> {
    let models_dir = state.settings.get().models_dir();
    let telemetry = state.telemetry.clone();
    let id = req.model.clone();
    let found = tokio::task::spawn_blocking(move || {
        let model = crate::models::scan(&models_dir).into_iter().find(|m| m.id == id)?;
        Some((model, telemetry.gpus()))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    let (model, gpus) = found.ok_or_else(|| ApiError::not_found("model not found"))?;
    let profile = state.profile();

    if model.format != "gguf" {
        return Ok(Json(TuneResult {
            args: vllm_args(&profile, &gpus),
            notes: vec!["safetensors モデルは vLLM 既定値を提案".to_string()],
            ..Default::default()
        }));
    }
    Ok(Json(recommend(&model, &gpus, &profile, req.ctx, &req.kv_type, req.parallel)))
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/tune", post(tune))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_profile() -> ModeProfile {
        ModeProfile {
            mode: Mode::Full,
            cpu_set: (0..48).collect(),
            physical_cores: 24,
            logical_cpus: 48,
            vram_reserve_mb: 512,
            gpu_power_percent: 100,
            ubatch_cap: 2048,
            nice: 0,
            max_concurrent_downloads: 2,
            connections_per_file: 8,
            download_bytes_per_sec: None,
            vmmem_priority: "Normal".into(),
        }
    }

    fn shared_profile() -> ModeProfile {
        ModeProfile {
            mode: Mode::Shared,
            cpu_set: (12..24).chain(36..48).collect(),
            physical_cores: 12,
            logical_cpus: 24,
            vram_reserve_mb: 4096,
            gpu_power_percent: 70,
            ubatch_cap: 256,
            nice: 10,
            max_concurrent_downloads: 1,
            connections_per_file: 4,
            download_bytes_per_sec: Some(40 * 1024 * 1024),
            vmmem_priority: "BelowNormal".into(),
        }
    }

    /// RTX 4090-class card with 24 GiB total and ~500 MiB already in use.
    fn gpu_24gb() -> Vec<GpuInfo> {
        vec![GpuInfo {
            name: "NVIDIA GeForce RTX 4090".into(),
            memory_total_mb: 24.0 * 1024.0,
            memory_used_mb: 500.0,
            memory_free_mb: 24.0 * 1024.0 - 500.0,
            ..Default::default()
        }]
    }

    fn dense_info() -> GgufInfo {
        GgufInfo {
            block_count: Some(64),
            head_count_kv: Some(8),
            key_length: Some(128),
            value_length: Some(128),
            context_length: Some(40960),
            has_chat_template: true,
            ..Default::default()
        }
    }

    fn model(size: u64, info: GgufInfo) -> LocalModel {
        LocalModel {
            id: "test/model.gguf".into(),
            format: "gguf".into(),
            path: "/models/test/model.gguf".into(),
            name: "model".into(),
            size,
            files: 1,
            mmproj: None,
            runtimes: vec!["llama.cpp".into()],
            info,
        }
    }

    /// Value following `flag`, if the flag is present.
    fn val<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).map(String::as_str)
    }

    fn has(args: &[String], flag: &str) -> bool {
        args.iter().any(|a| a == flag)
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn kv_scales_with_parallel() {
        let one = kv_cache_bytes(&dense_info(), 8192, "f16", 1);
        let four = kv_cache_bytes(&dense_info(), 8192, "f16", 4);
        assert_eq!(four, 4 * one);
        // 64 layers * 8192 ctx * 8 kv heads * 256 (k+v dims) * 2 bytes = 2 GiB
        assert_eq!(one, 2 * GIB);
    }

    #[test]
    fn kv_bytes_per_type() {
        let info = dense_info();
        let f16 = kv_cache_bytes(&info, 8192, "f16", 1);
        assert_eq!(kv_cache_bytes(&info, 8192, "q8_0", 1), (f16 as f64 * 1.0625 / 2.0) as u64);
        assert_eq!(kv_cache_bytes(&info, 8192, "q4_0", 1), (f16 as f64 * 0.28125) as u64);
        // Unknown cache types are costed like f16.
        assert_eq!(kv_cache_bytes(&info, 8192, "bogus", 1), f16);
    }

    #[test]
    fn fits_fully_on_gpu() {
        let p = full_profile();
        let r = recommend(&model(9 * GIB, dense_info()), &gpu_24gb(), &p, Some(16384), "q8_0", 1);
        let a = &r.args;
        assert_eq!(val(a, "-ngl"), Some("999"));
        assert_eq!(val(a, "-c"), Some("16384"));
        assert_eq!(val(a, "-t"), Some("4")); // GPU-resident: few CPU threads
        assert_eq!(val(a, "-tb"), Some("24"));
        assert_eq!(val(a, "-b"), Some("4096"));
        assert_eq!(val(a, "-ub"), Some("1024"));
        assert!(has(a, "-fa") && has(a, "--jinja") && has(a, "--metrics"));
        assert_eq!(val(a, "-ctk"), Some("q8_0"));
        assert!(!has(a, "--n-cpu-moe"));
        assert!(r.estimate.fits);
        assert_eq!(r.estimate.vram_reserve, 512 * MIB);
        assert_eq!(r.estimate.native_ctx, 40960);
        assert!(!r.notes.iter().any(|n| n.contains("共有モード")));
    }

    #[test]
    fn dense_partial_offload() {
        let p = full_profile();
        let r = recommend(&model(40 * GIB, dense_info()), &gpu_24gb(), &p, Some(8192), "q8_0", 1);
        let a = &r.args;
        let ngl: u64 = val(a, "-ngl").unwrap().parse().unwrap();
        assert!(0 < ngl && ngl < 64, "ngl = {ngl}");
        assert_eq!(val(a, "-t"), Some("24")); // physical cores
        assert_eq!(val(a, "-tb"), Some("48")); // logical cpus
        assert_eq!(val(a, "-b"), Some("2048"));
        assert_eq!(val(a, "-ub"), Some("512"));
        assert!(!has(a, "--n-cpu-moe"));
        assert!(!r.estimate.fits);
    }

    #[test]
    fn moe_uses_n_cpu_moe() {
        let p = full_profile();
        let info = GgufInfo { block_count: Some(48), head_count_kv: Some(4), expert_count: Some(128), expert_used_count: Some(8), ..dense_info() };
        let r = recommend(&model(60 * GIB, info), &gpu_24gb(), &p, Some(32768), "q8_0", 1);
        let a = &r.args;
        assert_eq!(val(a, "-ngl"), Some("999"));
        let n: u64 = val(a, "--n-cpu-moe").unwrap().parse().unwrap();
        assert!(0 < n && n <= 48, "n-cpu-moe = {n}");
        assert_eq!(val(a, "-t"), Some("24"));
    }

    #[test]
    fn no_gpu_means_cpu_only() {
        let p = full_profile();
        let r = recommend(&model(9 * GIB, dense_info()), &[], &p, None, "q8_0", 1);
        let a = &r.args;
        assert_eq!(val(a, "-ngl"), Some("0"));
        assert!(!has(a, "-fa") && !has(a, "-ctk"));
        assert_eq!(val(a, "-t"), Some("24"));
        assert_eq!(val(a, "-tb"), Some("48"));
        assert!(!r.estimate.fits);
        assert!(r.notes.iter().any(|n| n.contains("GPU 未検出")));
    }

    #[test]
    fn default_ctx_is_capped_at_32k_and_parallel_multiplies_c() {
        let p = full_profile();
        let r = recommend(&model(9 * GIB, dense_info()), &gpu_24gb(), &p, None, "f16", 2);
        assert_eq!(r.estimate.ctx, 32768);
        assert_eq!(val(&r.args, "-c"), Some("65536"));
        assert_eq!(val(&r.args, "-np"), Some("2"));
        assert!(!has(&r.args, "-ctk")); // f16 cache: no cache-type flags
    }

    #[test]
    fn shared_profile_limits_threads_ubatch_and_reserves_vram() {
        // 19 GiB fits in full mode (about 20 GiB of budget) but not in shared mode (about 17 GiB).
        let m = model(19 * GIB, dense_info());
        let full = recommend(&m, &gpu_24gb(), &full_profile(), Some(8192), "q8_0", 1);
        assert_eq!(val(&full.args, "-ngl"), Some("999"));
        assert!(full.estimate.fits);

        let shared = recommend(&m, &gpu_24gb(), &shared_profile(), Some(8192), "q8_0", 1);
        let a = &shared.args;
        let ngl: u64 = val(a, "-ngl").unwrap().parse().unwrap();
        assert!(ngl < 64, "ngl = {ngl}");
        assert!(!shared.estimate.fits);
        assert_eq!(shared.estimate.vram_reserve, 4096 * MIB);
        assert!(shared.estimate.vram_free < full.estimate.vram_free);
        assert_eq!(val(a, "-t"), Some("12"));
        assert_eq!(val(a, "-tb"), Some("24"));
        assert_eq!(val(a, "-ub"), Some("256"));
        assert_eq!(val(a, "-b"), Some("2048"));
        assert!(shared.notes.iter().any(|n| n.starts_with("共有モード: VRAM 4.0 GiB") && n.contains("CPU 12 コア (24 スレッド)") && n.ends_with("上限 256")));
    }

    #[test]
    fn shared_gpu_resident_caps_ubatch() {
        let r = recommend(&model(9 * GIB, dense_info()), &gpu_24gb(), &shared_profile(), Some(8192), "q8_0", 1);
        let a = &r.args;
        assert_eq!(val(a, "-ngl"), Some("999"));
        assert_eq!(val(a, "-t"), Some("4"));
        assert_eq!(val(a, "-tb"), Some("12"));
        assert_eq!(val(a, "-b"), Some("4096"));
        assert_eq!(val(a, "-ub"), Some("256"));
        assert!(r.estimate.fits);
    }

    #[test]
    fn vllm_args_full_and_shared() {
        let gpus = gpu_24gb();
        let full = vllm_args(&full_profile(), &gpus);
        assert_eq!(full, strs(&["--gpu-memory-utilization", "0.92", "--max-model-len", "32768", "--enable-prefix-caching"]));

        let shared = vllm_args(&shared_profile(), &gpus);
        assert_eq!(val(&shared, "--gpu-memory-utilization"), Some("0.79"));

        let mut squeezed = shared_profile();
        squeezed.vram_reserve_mb = 24000;
        assert_eq!(val(&vllm_args(&squeezed, &gpus), "--gpu-memory-utilization"), Some("0.30"));

        assert_eq!(val(&vllm_args(&shared_profile(), &[]), "--gpu-memory-utilization"), Some("0.92"));
    }

    #[test]
    fn merge_replaces_aliases_in_place() {
        let base = strs(&["-m", "a.gguf", "-ngl", "999", "-c", "4096"]);
        let over = strs(&["--n-gpu-layers", "20", "--ctx-size", "8192"]);
        assert_eq!(merge_args(&base, &over), strs(&["-m", "a.gguf", "--n-gpu-layers", "20", "--ctx-size", "8192"]));
        // Short form in overrides replaces the long form in base.
        let base = strs(&["--threads", "8", "--metrics"]);
        assert_eq!(merge_args(&base, &strs(&["-t", "4"])), strs(&["-t", "4", "--metrics"]));
    }

    #[test]
    fn merge_appends_new_flags_in_order() {
        let base = strs(&["-ngl", "999"]);
        let over = strs(&["--jinja", "-np", "4", "--mmproj", "vision.gguf"]);
        assert_eq!(merge_args(&base, &over), strs(&["-ngl", "999", "--jinja", "-np", "4", "--mmproj", "vision.gguf"]));
        assert_eq!(merge_args(&base, &[]), base);
    }

    #[test]
    fn merge_handles_valueless_flags() {
        // A value-less override replaces a flag that had a value, and vice versa.
        let base = strs(&["--metrics", "-fa", "on"]);
        assert_eq!(merge_args(&base, &strs(&["--flash-attn"])), strs(&["--metrics", "--flash-attn"]));
        let base = strs(&["--jinja", "--metrics"]);
        assert_eq!(merge_args(&base, &strs(&["--jinja"])), strs(&["--jinja", "--metrics"]));
    }

    #[test]
    fn merge_keeps_negative_numbers_as_values() {
        let base = strs(&["--metrics", "-ngl", "-1"]);
        assert_eq!(merge_args(&base, &strs(&["-ngl", "20"])), strs(&["--metrics", "-ngl", "20"]));
        // "-1" is a value, not a flag, so it is not mistaken for a separate flag to append.
        assert_eq!(merge_args(&strs(&["-ngl", "999"]), &strs(&["-ngl", "-1"])), strs(&["-ngl", "-1"]));
        assert_eq!(merge_args(&[], &strs(&["--jinja", "-ngl", "-1"])), strs(&["--jinja", "-ngl", "-1"]));
    }

    #[test]
    fn merge_accepts_inline_values() {
        let base = strs(&["-c", "4096", "--metrics"]);
        assert_eq!(merge_args(&base, &strs(&["--ctx-size=8192"])), strs(&["--ctx-size", "8192", "--metrics"]));
    }

    #[test]
    fn tune_request_defaults() {
        let req: TuneReq = serde_json::from_str(r#"{"model":"a/b.gguf"}"#).unwrap();
        assert_eq!(req.model, "a/b.gguf");
        assert_eq!(req.ctx, None);
        assert_eq!(req.kv_type, "q8_0");
        assert_eq!(req.parallel, 1);
    }
}
