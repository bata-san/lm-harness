//! Host telemetry: GPU via NVML (fallback: nvidia-smi), CPU/mem/disk via sysinfo, WSL health checks.
//! Routes: GET /api/system, GET /api/system/checks

use std::ffi::{CString, OsStr};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::Nvml;
use parking_lot::Mutex;
use sysinfo::{MemoryRefreshKind, System};

use crate::config::Settings;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::types::{Check, GpuInfo};

const WSL_NVML_LIB: &str = "/usr/lib/wsl/lib/libnvidia-ml.so.1";
const WSL_NVIDIA_SMI: &str = "/usr/lib/wsl/lib/nvidia-smi";
/// Several pollers hit the GPU readings each second; reuse a reading this long.
const GPU_CACHE_TTL: Duration = Duration::from_millis(700);
const SMI_TIMEOUT: Duration = Duration::from_secs(5);
const GIB: u64 = 1024 * 1024 * 1024;

pub struct Telemetry {
    nvml: Option<Nvml>,
    sys: Mutex<System>,
    gpu_cache: Mutex<Option<(Instant, Vec<GpuInfo>)>>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl Telemetry {
    pub fn new() -> Self {
        let nvml = Nvml::init()
            .or_else(|_| Nvml::builder().lib_path(OsStr::new(WSL_NVML_LIB)).init())
            .map_err(|e| tracing::debug!("NVML unavailable, will fall back to nvidia-smi: {e}"))
            .ok();
        let mut sys = System::new();
        // Prime CPU usage so the first real snapshot has a baseline to diff against.
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Self { nvml, sys: Mutex::new(sys), gpu_cache: Mutex::new(None) }
    }

    /// Current GPU readings (empty if no NVIDIA GPU / driver). Cached for ~700 ms.
    pub fn gpus(&self) -> Vec<GpuInfo> {
        // Holding the lock while refreshing makes concurrent callers wait for one reading instead of all querying.
        let mut cache = self.gpu_cache.lock();
        if let Some((at, v)) = cache.as_ref() {
            if at.elapsed() < GPU_CACHE_TTL {
                return v.clone();
            }
        }
        let fresh = self.read_gpus();
        *cache = Some((Instant::now(), fresh.clone()));
        fresh
    }

    fn read_gpus(&self) -> Vec<GpuInfo> {
        if let Some(nvml) = &self.nvml {
            if let Some(v) = nvml_gpus(nvml) {
                return v;
            }
        }
        smi_gpus()
    }

    /// JSON for GET /api/system (same shape as before: gpus, cpu, memory, swap, disk).
    pub fn snapshot(&self, models_dir: &Path) -> serde_json::Value {
        let gpus = self.gpus();
        let topo = crate::topology::cpu_topology();
        let (percent, per_core, freq, mem_total, mem_avail, swap_total, swap_used) = {
            let mut s = self.sys.lock();
            s.refresh_cpu_usage();
            s.refresh_cpu_frequency();
            s.refresh_memory_specifics(MemoryRefreshKind::everything());
            let per_core: Vec<f64> = s.cpus().iter().map(|c| round1(c.cpu_usage() as f64)).collect();
            let freq = s.cpus().iter().map(|c| c.frequency()).max().filter(|f| *f > 0);
            (
                round1(s.global_cpu_usage() as f64),
                per_core,
                freq,
                s.total_memory(),
                s.available_memory(),
                s.total_swap(),
                s.used_swap(),
            )
        };
        let used = mem_total.saturating_sub(mem_avail);
        let mem_percent = if mem_total > 0 { round1(used as f64 * 100.0 / mem_total as f64) } else { 0.0 };
        let (d_total, d_used, d_free) = disk_usage(models_dir).unwrap_or((0, 0, 0));
        let load = load_avg();
        serde_json::json!({
            "gpus": gpus,
            "cpu": {
                "model": topo.model,
                "physical_cores": topo.physical_cores,
                "logical_cores": topo.logical_cores,
                "percent": percent,
                "per_core": per_core,
                "freq_mhz": freq,
                "load_avg": load,
            },
            "memory": { "total": mem_total, "used": used, "available": mem_avail, "percent": mem_percent },
            "swap": { "total": swap_total, "used": swap_used },
            "disk": { "path": models_dir.to_string_lossy(), "total": d_total, "used": d_used, "free": d_free },
        })
    }

    pub fn health_checks(&self, settings: &Settings) -> Vec<Check> {
        let mut checks = Vec::new();
        let wsl = is_wsl();
        checks.push(Check::new(
            "wsl",
            wsl,
            "info",
            if wsl { "WSL2 上で動作中" } else { "WSL ではないネイティブ Linux で動作中" },
        ));
        if wsl {
            checks.push(wsl_systemd_check(std::fs::read_to_string("/etc/wsl.conf").ok().as_deref()));
        }

        let gs = self.gpus();
        checks.push(Check::new(
            "gpu",
            !gs.is_empty(),
            "error",
            if gs.is_empty() {
                "nvidia-smi が見つからない / GPU 未検出 (Windows 側 NVIDIA ドライバを確認)".to_string()
            } else {
                format!("GPU 検出: {}", gs.iter().map(|g| g.name.as_str()).collect::<Vec<_>>().join(", "))
            },
        ));

        let libcuda = Path::new("/usr/lib/x86_64-linux-gnu/libcuda.so.1");
        if wsl && libcuda.exists() {
            let real = std::fs::canonicalize(libcuda).unwrap_or_else(|_| libcuda.to_path_buf());
            if !real.starts_with("/usr/lib/wsl/") {
                checks.push(Check::new(
                    "libcuda",
                    false,
                    "warn",
                    "WSL 内に Linux 用 libcuda が入っています。WSL では Windows ドライバの /usr/lib/wsl/lib を使うべきです",
                ));
            }
        }

        let nvcc = find_in_path("nvcc").or_else(|| {
            let p = PathBuf::from("/usr/local/cuda/bin/nvcc");
            p.exists().then_some(p)
        });
        checks.push(Check::new(
            "nvcc",
            nvcc.is_some(),
            "warn",
            if nvcc.is_some() {
                "CUDA Toolkit (nvcc) あり"
            } else {
                "nvcc なし: llama.cpp の CUDA ビルドに必要 (scripts/wsl/setup.sh)"
            },
        ));

        let mdir = settings.models_dir();
        let mdir_s = mdir.to_string_lossy().to_string();
        let on_mnt = mdir_s.starts_with("/mnt/");
        checks.push(Check::new(
            "models_fs",
            !on_mnt,
            "warn",
            if on_mnt {
                format!("モデル置き場 {mdir_s} は Windows ドライブ (9P) 上で遅い。~/models など ext4 側を推奨")
            } else {
                format!("モデル置き場 {mdir_s} は Linux FS 上 (高速)")
            },
        ));

        let total = {
            let mut s = self.sys.lock();
            s.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
            s.total_memory()
        };
        checks.push(memory_check(total));

        let topo = crate::topology::cpu_topology();
        checks.push(Check::new(
            "cpu",
            true,
            "info",
            format!("{} / {}C{}T が WSL から見えています", topo.model, topo.physical_cores, topo.logical_cores),
        ));

        if let Ok(g) = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor") {
            let g = g.trim();
            checks.push(Check::new("governor", g == "performance", "info", format!("CPU governor: {g}")));
        }
        checks
    }
}

fn memory_check(total: u64) -> Check {
    let ok = total >= 48 * GIB;
    let mut msg = format!("WSL に割り当て済みメモリ {:.0} GiB", total as f64 / GIB as f64);
    if !ok {
        msg.push_str(" — .wslconfig の memory= を増やすと大きいモデルの CPU オフロードが安定");
    }
    Check::new("memory", ok, "warn", msg)
}

/// Info-level check on `/etc/wsl.conf`: is `[boot] systemd=true` set? `None` content = file missing.
fn wsl_systemd_check(wsl_conf: Option<&str>) -> Check {
    let enabled = wsl_conf.is_some_and(wsl_conf_systemd);
    Check::new(
        "wsl_systemd",
        enabled,
        "info",
        if enabled {
            "/etc/wsl.conf: systemd=true (有効)"
        } else {
            "/etc/wsl.conf に [boot] systemd=true がありません (ブート時の自動起動や一部サービスを使う場合に推奨)"
        },
    )
}

/// Parse `[boot]\nsystemd=true` out of wsl.conf contents.
fn wsl_conf_systemd(conf: &str) -> bool {
    let mut in_boot = false;
    for line in conf.lines() {
        let line = line.split(['#', ';']).next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(sec) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_boot = sec.trim().eq_ignore_ascii_case("boot");
            continue;
        }
        if in_boot {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim().eq_ignore_ascii_case("systemd") {
                    return v.trim().trim_matches('"').eq_ignore_ascii_case("true");
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------- helpers

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn load_avg() -> [f64; 3] {
    let l = System::load_average();
    [round2(l.one), round2(l.five), round2(l.fifteen)]
}

fn find_in_path(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(bin))
        .find(|p| p.is_file())
}

/// `(total, used, free)` in bytes for the filesystem holding `path` (nearest existing ancestor if it is missing).
fn disk_usage(path: &Path) -> Option<(u64, u64, u64)> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated string and `st` is a properly sized out-parameter.
    let st = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        st
    };
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64 * frsize;
    let free = st.f_bavail as u64 * frsize;
    let used = (st.f_blocks as u64).saturating_sub(st.f_bfree as u64) * frsize;
    Some((total, used, free))
}

static WSL: LazyLock<bool> = LazyLock::new(|| {
    std::fs::read_to_string("/proc/version").map(|s| s.to_lowercase().contains("microsoft")).unwrap_or(false)
});

pub fn is_wsl() -> bool {
    *WSL
}

// ---------------------------------------------------------------- NVML

/// Read all devices via NVML; `None` if the device count itself is unavailable (caller falls back to nvidia-smi).
fn nvml_gpus(nvml: &Nvml) -> Option<Vec<GpuInfo>> {
    let count = nvml.device_count().ok()?;
    let driver = nvml.sys_driver_version().unwrap_or_default();
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let Ok(dev) = nvml.device_by_index(i) else { continue };
        let mut g = GpuInfo { index: i, driver: driver.clone(), ..Default::default() };
        g.name = dev.name().unwrap_or_default();
        if let Ok(m) = dev.memory_info() {
            const MB: f64 = 1024.0 * 1024.0;
            g.memory_total_mb = m.total as f64 / MB;
            g.memory_used_mb = m.used as f64 / MB;
            g.memory_free_mb = m.free as f64 / MB;
        }
        if let Ok(u) = dev.utilization_rates() {
            g.util_gpu = u.gpu as f64;
            g.util_mem = u.memory as f64;
        }
        g.temperature = dev.temperature(TemperatureSensor::Gpu).map(f64::from).unwrap_or(0.0);
        g.power_draw = dev.power_usage().ok().map(|mw| mw as f64 / 1000.0);
        g.power_limit = dev.enforced_power_limit().ok().map(|mw| mw as f64 / 1000.0);
        g.power_default_limit = dev.power_management_limit_default().ok().map(|mw| mw as f64 / 1000.0);
        g.clock_sm = dev.clock_info(Clock::SM).map(f64::from).unwrap_or(0.0);
        g.clock_sm_max = dev.max_clock_info(Clock::SM).map(f64::from).unwrap_or(0.0);
        g.pcie_gen = dev.current_pcie_link_gen().map(|v| v.to_string()).unwrap_or_default();
        g.pcie_width = dev.current_pcie_link_width().map(|v| v.to_string()).unwrap_or_default();
        out.push(g);
    }
    Some(out)
}

// ---------------------------------------------------------------- nvidia-smi fallback

const SMI_FIELDS: &[&str] = &[
    "index",
    "name",
    "memory.total",
    "memory.used",
    "memory.free",
    "utilization.gpu",
    "utilization.memory",
    "temperature.gpu",
    "power.draw",
    "power.limit",
    "power.default_limit",
    "clocks.sm",
    "clocks.max.sm",
    "pcie.link.gen.current",
    "pcie.link.width.current",
    "driver_version",
];

fn nvidia_smi_path() -> Option<&'static PathBuf> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| {
        find_in_path("nvidia-smi").or_else(|| {
            let p = PathBuf::from(WSL_NVIDIA_SMI);
            p.exists().then_some(p)
        })
    })
    .as_ref()
}

fn smi_gpus() -> Vec<GpuInfo> {
    let Some(exe) = nvidia_smi_path() else { return vec![] };
    let mut child = match Command::new(exe)
        .arg(format!("--query-gpu={}", SMI_FIELDS.join(",")))
        .arg("--format=csv,noheader,nounits")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    // Output is tiny (fits the pipe buffer), so waiting before reading is safe; poll for the timeout.
    let deadline = Instant::now() + SMI_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return vec![];
            }
        }
    }
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        let _ = so.read_to_string(&mut out);
    }
    parse_smi_csv(&out)
}

/// `[N/A]`, `[Not Supported]`, empty, ... -> `None`.
fn smi_num(v: &str) -> Option<f64> {
    v.trim().parse::<f64>().ok().filter(|x| x.is_finite())
}

fn smi_str(v: &str) -> String {
    let v = v.trim();
    if v.starts_with('[') && v.ends_with(']') {
        String::new()
    } else {
        v.to_string()
    }
}

/// Parse `nvidia-smi --query-gpu=<SMI_FIELDS> --format=csv,noheader,nounits` output.
fn parse_smi_csv(out: &str) -> Vec<GpuInfo> {
    let n = SMI_FIELDS.len();
    let mut result = Vec::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let mut vals: Vec<&str> = line.split(',').map(str::trim).collect();
        if vals.len() < n {
            continue;
        }
        // A GPU name containing commas spills into extra columns; glue them back together.
        let name = if vals.len() > n {
            let extra = vals.len() - n;
            let joined = vals[1..=1 + extra].join(", ");
            vals.drain(1..=1 + extra);
            vals.insert(1, "");
            joined
        } else {
            vals[1].to_string()
        };
        let Ok(index) = vals[0].parse::<u32>() else { continue };
        result.push(GpuInfo {
            index,
            name,
            memory_total_mb: smi_num(vals[2]).unwrap_or(0.0),
            memory_used_mb: smi_num(vals[3]).unwrap_or(0.0),
            memory_free_mb: smi_num(vals[4]).unwrap_or(0.0),
            util_gpu: smi_num(vals[5]).unwrap_or(0.0),
            util_mem: smi_num(vals[6]).unwrap_or(0.0),
            temperature: smi_num(vals[7]).unwrap_or(0.0),
            power_draw: smi_num(vals[8]),
            power_limit: smi_num(vals[9]),
            power_default_limit: smi_num(vals[10]),
            clock_sm: smi_num(vals[11]).unwrap_or(0.0),
            clock_sm_max: smi_num(vals[12]).unwrap_or(0.0),
            pcie_gen: smi_str(vals[13]),
            pcie_width: smi_str(vals[14]),
            driver: smi_str(vals[15]),
        });
    }
    result
}

// ---------------------------------------------------------------- routes

pub fn router() -> Router<AppState> {
    Router::new().route("/api/system", get(get_system)).route("/api/system/checks", get(get_checks))
}

async fn get_system(State(st): State<AppState>) -> ApiResult<serde_json::Value> {
    let dir = st.settings.get().models_dir();
    let tel = st.telemetry.clone();
    let v = tokio::task::spawn_blocking(move || tel.snapshot(&dir))
        .await
        .map_err(|e| ApiError::internal(format!("system snapshot failed: {e}")))?;
    Ok(Json(v))
}

async fn get_checks(State(st): State<AppState>) -> ApiResult<Vec<Check>> {
    let settings = st.settings.get();
    let tel = st.telemetry.clone();
    let v = tokio::task::spawn_blocking(move || tel.health_checks(&settings))
        .await
        .map_err(|e| ApiError::internal(format!("health checks failed: {e}")))?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "0, NVIDIA GeForce RTX 5090, 32607, 1234, 31000, 12, 3, 45, 75.32, 575.00, 600.00, 2400, 3090, 5, 16, 576.80";

    #[test]
    fn parses_full_line() {
        let g = parse_smi_csv(&format!("{LINE}\n"));
        assert_eq!(g.len(), 1);
        let g = &g[0];
        assert_eq!(g.index, 0);
        assert_eq!(g.name, "NVIDIA GeForce RTX 5090");
        assert_eq!(g.memory_total_mb, 32607.0);
        assert_eq!(g.memory_used_mb, 1234.0);
        assert_eq!(g.util_gpu, 12.0);
        assert_eq!(g.temperature, 45.0);
        assert_eq!(g.power_draw, Some(75.32));
        assert_eq!(g.power_limit, Some(575.0));
        assert_eq!(g.power_default_limit, Some(600.0));
        assert_eq!(g.clock_sm_max, 3090.0);
        assert_eq!(g.pcie_gen, "5");
        assert_eq!(g.pcie_width, "16");
        assert_eq!(g.driver, "576.80");
    }

    #[test]
    fn handles_na_and_multiple_gpus() {
        let na = "1, RTX A, 24576, 1, 24575, 0, 0, 30, [N/A], [Not Supported], [N/A], 210, 2100, [N/A], 8, 550.1";
        let g = parse_smi_csv(&format!("{LINE}\n\n{na}\n"));
        assert_eq!(g.len(), 2);
        assert_eq!(g[1].index, 1);
        assert_eq!(g[1].power_draw, None);
        assert_eq!(g[1].power_limit, None);
        assert_eq!(g[1].power_default_limit, None);
        assert_eq!(g[1].pcie_gen, "");
        assert_eq!(g[1].pcie_width, "8");
    }

    #[test]
    fn skips_garbage_and_joins_comma_names() {
        assert!(parse_smi_csv("NVIDIA-SMI has failed\n").is_empty());
        assert!(parse_smi_csv("").is_empty());
        let line = LINE.replace("NVIDIA GeForce RTX 5090", "Weird, Name");
        let g = parse_smi_csv(&line);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].name, "Weird, Name");
        assert_eq!(g[0].driver, "576.80");
    }

    #[test]
    fn wsl_conf_parsing() {
        assert!(wsl_conf_systemd("[boot]\nsystemd=true\n"));
        assert!(wsl_conf_systemd("[network]\nhostname=x\n[boot]\n systemd = True # c\n"));
        assert!(!wsl_conf_systemd("[boot]\nsystemd=false\n"));
        assert!(!wsl_conf_systemd("[user]\nsystemd=true\n"));
        assert!(!wsl_conf_systemd(""));
        assert!(!wsl_systemd_check(None).ok);
        assert!(wsl_systemd_check(Some("[boot]\nsystemd=true")).ok);
    }

    #[test]
    fn memory_check_threshold() {
        assert!(memory_check(64 * GIB).ok);
        let c = memory_check(16 * GIB);
        assert!(!c.ok && c.message.contains("16 GiB") && c.message.contains(".wslconfig"));
    }

    #[test]
    fn disk_usage_of_missing_path_uses_ancestor() {
        let (t, u, f) = disk_usage(Path::new("/tmp/definitely/not/here/xyz")).unwrap();
        assert!(t > 0 && u <= t && f <= t);
    }

    #[test]
    fn snapshot_shape() {
        let t = Telemetry::new();
        let v = t.snapshot(Path::new("/tmp"));
        assert!(v["gpus"].is_array());
        assert!(v["cpu"]["per_core"].is_array());
        assert_eq!(v["cpu"]["load_avg"].as_array().unwrap().len(), 3);
        assert!(v["memory"]["total"].as_u64().unwrap() > 0);
        assert_eq!(v["disk"]["path"], "/tmp");
        assert_eq!(round1(12.34), 12.3);
    }
}
