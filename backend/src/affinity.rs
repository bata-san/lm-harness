//! CPU affinity / priority control for inference processes (Linux only).
//!
//! Inside WSL2, pinning inference to a subset of vCPUs leaves the remaining vCPUs idle, so
//! Hyper-V hands that host CPU time back to Windows applications.

use std::io;

/// Pin every thread of `pid` to `cpus`.
pub fn set_affinity(pid: i32, cpus: &[usize]) -> io::Result<()> {
    let set = cpu_set(cpus);
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task"))?;
    for t in tasks.flatten() {
        if let Some(tid) = t.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) {
            // SAFETY: plain syscall with a fully initialised cpu_set_t.
            unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set) };
        }
    }
    Ok(())
}

/// Set the nice value of every thread of `pid` (Linux nice is per thread).
pub fn set_nice(pid: i32, nice: i32) {
    if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for t in tasks.flatten() {
            if let Some(tid) = t.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) {
                // SAFETY: setpriority on a tid we just listed; errors are ignored (raising
                // priority back from >0 to 0 needs CAP_SYS_NICE and is best effort).
                unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) };
            }
        }
    }
}

/// All descendants of `root` (children, grandchildren, ...), not including `root`.
pub fn descendants(root: i32) -> Vec<i32> {
    let mut parent_of: Vec<(i32, i32)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                // comm may contain spaces/parens: fields after the last ')' are stable
                if let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) {
                    if let Some(ppid) = rest.split_whitespace().nth(1).and_then(|s| s.parse::<i32>().ok()) {
                        parent_of.push((pid, ppid));
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for &(pid, ppid) in &parent_of {
            if ppid == p {
                out.push(pid);
                frontier.push(pid);
            }
        }
    }
    out
}

/// Apply affinity + nice to `root` and all its descendants. Best effort.
pub fn apply_tree(root: i32, cpus: &[usize], nice: i32) {
    for pid in std::iter::once(root).chain(descendants(root)) {
        let _ = set_affinity(pid, cpus);
        set_nice(pid, nice);
    }
}

/// Closure for `std::os::unix::process::CommandExt::pre_exec` / tokio `Command::pre_exec`:
/// the child starts already pinned and deprioritised, so threads it spawns inherit both.
pub fn child_setup(cpus: Vec<usize>, nice: i32) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    let set = cpu_set(&cpus);
    move || {
        // SAFETY: only async-signal-safe syscalls between fork and exec.
        unsafe {
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
            if nice != 0 {
                libc::setpriority(libc::PRIO_PROCESS, 0, nice);
            }
        }
        Ok(())
    }
}

fn cpu_set(cpus: &[usize]) -> libc::cpu_set_t {
    // SAFETY: cpu_set_t is a plain bitmask; zeroed is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &c in cpus {
        if c < libc::CPU_SETSIZE as usize {
            unsafe { libc::CPU_SET(c, &mut set) };
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_self_and_restore() {
        let me = std::process::id() as i32;
        let all: Vec<usize> = (0..std::thread::available_parallelism().unwrap().get()).collect();
        set_affinity(me, &all[..1]).unwrap();
        set_affinity(me, &all).unwrap();
    }
}
