//! Performance modes.
//!
//! * `Full`   — the workstation is dedicated to inference: every core, all VRAM, stock GPU
//!              power limit, unthrottled downloads.
//! * `Shared` — someone is using the workstation: inference is pinned to a subset of cores
//!              with a lower priority, VRAM is reserved for the desktop, the GPU power limit
//!              is lowered, smaller micro-batches keep the desktop compositor responsive and
//!              downloads are rate limited.
//!
//! A `ModeProfile` is the fully resolved set of limits for the current mode. It is broadcast
//! through a `tokio::sync::watch` channel (see `AppState::mode_rx`) so every subsystem reacts
//! to switches without polling.

use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::types::CpuTopology;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Full,
    Shared,
}

/// User-tunable limits applied in shared mode.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedLimits {
    /// Fraction of physical cores given to inference (0.1..=1.0).
    pub cpu_fraction: f32,
    /// VRAM kept free for the Windows desktop / other apps.
    pub vram_reserve_mb: u64,
    /// GPU power limit as % of the board's default limit (100 = unchanged).
    pub gpu_power_percent: u32,
    /// Cap for llama.cpp `-ub`; small micro-batches let the desktop get GPU time slices.
    pub ubatch_cap: u32,
    /// Linux nice value for inference processes (0..=19).
    pub nice: i32,
    pub max_concurrent_downloads: usize,
    pub connections_per_file: usize,
    /// Download cap in MiB/s across all transfers (None = unlimited).
    pub download_mib_per_sec: Option<u64>,
    /// Lower the Windows priority of the WSL VM process (vmmemWSL) so foreground apps win.
    pub vmmem_below_normal: bool,
}

impl Default for SharedLimits {
    fn default() -> Self {
        Self {
            cpu_fraction: 0.5,
            vram_reserve_mb: 4096,
            gpu_power_percent: 70,
            ubatch_cap: 256,
            nice: 10,
            max_concurrent_downloads: 1,
            connections_per_file: 4,
            download_mib_per_sec: Some(40),
            vmmem_below_normal: true,
        }
    }
}

/// Time-based automatic switching (local time of the WSL distro).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub enabled: bool,
    /// "HH:MM" — start of the full-performance window.
    pub full_start: String,
    /// "HH:MM" — end of the full-performance window (may wrap past midnight).
    pub full_end: String,
    /// Saturday and Sunday are full all day.
    pub weekends_full: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Self { enabled: false, full_start: "20:00".into(), full_end: "08:00".into(), weekends_full: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeSettings {
    /// Last selected mode (restored on startup).
    pub current: Mode,
    /// On a switch, restart servers launched with `auto_tune` so thread counts / batch sizes
    /// match the new limits (CPU affinity and priority are always applied live).
    pub auto_restart: bool,
    pub shared: SharedLimits,
    pub schedule: Schedule,
}

impl Default for ModeSettings {
    fn default() -> Self {
        Self { current: Mode::Full, auto_restart: true, shared: SharedLimits::default(), schedule: Schedule::default() }
    }
}

/// Fully resolved limits for one mode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModeProfile {
    pub mode: Mode,
    /// Logical CPU ids inference processes may run on.
    pub cpu_set: Vec<usize>,
    /// Physical cores covered by `cpu_set` (use for llama.cpp `-t`).
    pub physical_cores: usize,
    /// `cpu_set.len()` (use for llama.cpp `-tb`).
    pub logical_cpus: usize,
    pub vram_reserve_mb: u64,
    pub gpu_power_percent: u32,
    pub ubatch_cap: u32,
    pub nice: i32,
    pub max_concurrent_downloads: usize,
    pub connections_per_file: usize,
    pub download_bytes_per_sec: Option<u64>,
    /// Windows priority class for vmmemWSL: "Normal" | "BelowNormal".
    pub vmmem_priority: String,
}

/// Resolve the concrete limits for `mode` on this machine.
pub fn resolve(mode: Mode, settings: &Settings, topo: &CpuTopology) -> ModeProfile {
    let all_cores: Vec<&Vec<usize>> = topo.cores.iter().collect();
    match mode {
        Mode::Full => {
            let cpu_set: Vec<usize> = (0..topo.logical_cores).collect();
            ModeProfile {
                mode,
                logical_cpus: cpu_set.len(),
                cpu_set,
                physical_cores: topo.physical_cores,
                vram_reserve_mb: 512,
                gpu_power_percent: 100,
                ubatch_cap: 2048,
                nice: 0,
                max_concurrent_downloads: settings.max_concurrent_downloads.max(1),
                connections_per_file: settings.connections_per_file.max(1),
                download_bytes_per_sec: None,
                vmmem_priority: "Normal".into(),
            }
        }
        Mode::Shared => {
            let s = &settings.mode.shared;
            let frac = s.cpu_fraction.clamp(0.05, 1.0);
            let n = ((all_cores.len() as f32 * frac).round() as usize).clamp(1, all_cores.len().max(1));
            // Take cores from the top: Windows (and most interactive apps) favour low-numbered CPUs.
            let mut cpu_set: Vec<usize> = all_cores.iter().rev().take(n).flat_map(|c| c.iter().copied()).collect();
            if cpu_set.is_empty() {
                cpu_set = vec![0];
            }
            cpu_set.sort_unstable();
            ModeProfile {
                mode,
                logical_cpus: cpu_set.len(),
                cpu_set,
                physical_cores: n,
                vram_reserve_mb: s.vram_reserve_mb,
                gpu_power_percent: s.gpu_power_percent.clamp(30, 100),
                ubatch_cap: s.ubatch_cap.max(32),
                nice: s.nice.clamp(0, 19),
                max_concurrent_downloads: s.max_concurrent_downloads.max(1),
                connections_per_file: s.connections_per_file.max(1),
                download_bytes_per_sec: s.download_mib_per_sec.filter(|v| *v > 0).map(|v| v * 1024 * 1024),
                vmmem_priority: if s.vmmem_below_normal { "BelowNormal" } else { "Normal" }.into(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topo() -> CpuTopology {
        // 24C/48T, Linux-style numbering: core i -> [i, i+24]
        CpuTopology {
            model: "AMD Ryzen Threadripper PRO 5965WX".into(),
            physical_cores: 24,
            logical_cores: 48,
            cores: (0..24).map(|i| vec![i, i + 24]).collect(),
        }
    }

    #[test]
    fn full_uses_everything() {
        let p = resolve(Mode::Full, &Settings::default(), &topo());
        assert_eq!(p.cpu_set.len(), 48);
        assert_eq!(p.physical_cores, 24);
        assert_eq!(p.download_bytes_per_sec, None);
    }

    #[test]
    fn shared_takes_whole_cores_from_the_top() {
        let p = resolve(Mode::Shared, &Settings::default(), &topo());
        assert_eq!(p.physical_cores, 12);
        assert_eq!(p.logical_cpus, 24);
        assert!(p.cpu_set.contains(&23) && p.cpu_set.contains(&47));
        assert!(!p.cpu_set.contains(&0) && !p.cpu_set.contains(&24));
        assert_eq!(p.download_bytes_per_sec, Some(40 * 1024 * 1024));
    }
}
