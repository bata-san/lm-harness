//! CPU topology from sysfs (works inside WSL2, which exposes the Hyper-V vCPU topology).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::types::CpuTopology;

static TOPOLOGY: LazyLock<CpuTopology> = LazyLock::new(detect);

pub fn cpu_topology() -> &'static CpuTopology {
    &TOPOLOGY
}

fn detect() -> CpuTopology {
    let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    // (package, core_id) -> logical cpus
    let mut cores: BTreeMap<(u32, u32), Vec<usize>> = BTreeMap::new();
    for cpu in 0..logical.max(online_cpus()) {
        let base = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
        let read = |f: &str| std::fs::read_to_string(format!("{base}/{f}")).ok().and_then(|s| s.trim().parse::<u32>().ok());
        match (read("physical_package_id"), read("core_id")) {
            (Some(pkg), Some(core)) => cores.entry((pkg, core)).or_default().push(cpu),
            _ => {
                cores.insert((u32::MAX, cpu as u32), vec![cpu]);
            }
        }
    }
    let mut cores: Vec<Vec<usize>> = cores.into_values().collect();
    for c in &mut cores {
        c.sort_unstable();
    }
    cores.sort_by_key(|c| c[0]);
    let logical_cores = cores.iter().map(Vec::len).sum::<usize>().max(1);
    CpuTopology { model: cpu_model(), physical_cores: cores.len().max(1), logical_cores, cores }
}

fn online_cpus() -> usize {
    // "0-47" or "0-3,8-11"
    std::fs::read_to_string("/sys/devices/system/cpu/online")
        .ok()
        .map(|s| {
            s.trim()
                .split(',')
                .filter_map(|r| r.split('-').next_back()?.parse::<usize>().ok())
                .max()
                .map_or(0, |m| m + 1)
        })
        .unwrap_or(0)
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string()))
        .unwrap_or_else(|| "unknown".into())
}
