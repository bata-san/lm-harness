//! Supervised child processes with captured logs (used by instances and jobs).
//!
//! Each child runs in its own process group (so `stop()` also takes down its children),
//! starts pinned to a CPU set / nice value (see `affinity::child_setup`) and has stdout and
//! stderr merged into one ring buffer of numbered lines.

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::affinity;

/// Lines kept per process.
pub const LOG_CAP: usize = 4000;
/// A single line longer than this is flushed as-is (guards against unbounded buffering).
const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Starting,
    Running,
    Exited,
    Failed,
    Stopped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Exited => "exited",
            Status::Failed => "failed",
            Status::Stopped => "stopped",
        }
    }
    pub fn is_alive(self) -> bool {
        matches!(self, Status::Starting | Status::Running)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct LogLine {
    pub seq: u64,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct LogTail {
    pub lines: Vec<LogLine>,
    pub last: u64,
}

pub fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Incremental line splitter: `\n` and `\r` both end a line (progress bars use `\r`),
/// blank lines are dropped and invalid UTF-8 is replaced.
#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' || b == b'\r' {
                self.flush(&mut out);
            } else {
                self.buf.push(b);
                if self.buf.len() >= MAX_LINE_BYTES {
                    self.flush(&mut out);
                }
            }
        }
        out
    }

    /// Remaining partial line at EOF.
    pub fn finish(&mut self) -> Option<String> {
        let mut out = Vec::new();
        self.flush(&mut out);
        out.pop()
    }

    fn flush(&mut self, out: &mut Vec<String>) {
        if !self.buf.iter().all(u8::is_ascii_whitespace) {
            out.push(String::from_utf8_lossy(&self.buf).into_owned());
        }
        self.buf.clear();
    }
}

struct State {
    status: Status,
    pid: Option<u32>,
    returncode: Option<i32>,
    ended_at: Option<f64>,
    logs: VecDeque<(u64, String)>,
    seq: u64,
    stopping: bool,
}

pub struct Proc {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub cmd: Vec<String>,
    pub meta: Map<String, Value>,
    pub started_at: f64,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    pin: Option<(Vec<usize>, i32)>,
    state: Mutex<State>,
}

impl Proc {
    pub fn new(name: impl Into<String>, cmd: Vec<String>, env: BTreeMap<String, String>, cwd: Option<String>, kind: &str, meta: Map<String, Value>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string()[..10].to_string(),
            name: name.into(),
            kind: kind.to_string(),
            cmd,
            meta,
            started_at: now_secs(),
            env,
            cwd,
            pin: None,
            state: Mutex::new(State { status: Status::Starting, pid: None, returncode: None, ended_at: None, logs: VecDeque::new(), seq: 0, stopping: false }),
        }
    }

    /// Start the child already pinned to `cpus` with the given nice value.
    pub fn pinned(mut self, cpus: Vec<usize>, nice: i32) -> Self {
        self.pin = Some((cpus, nice));
        self
    }

    /// Spawn the child. A spawn failure is not an error: the proc ends up `failed` with a log line.
    pub async fn start(self) -> std::sync::Arc<Self> {
        let this = std::sync::Arc::new(self);
        this.log(format!("$ {}", this.cmd.join(" ")));
        match this.spawn() {
            Ok(child) => this.supervise(child),
            Err(e) => {
                this.log(format!("[harness] 起動失敗: {e}"));
                let mut st = this.state.lock();
                st.status = Status::Failed;
                st.ended_at = Some(now_secs());
            }
        }
        this
    }

    fn spawn(&self) -> std::io::Result<tokio::process::Child> {
        let Some((prog, args)) = self.cmd.split_first() else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "空のコマンド"));
        };
        let mut cmd = Command::new(prog);
        cmd.args(args).envs(&self.env).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).kill_on_drop(false);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        if let Some((cpus, nice)) = &self.pin {
            // SAFETY: the closure only performs async-signal-safe syscalls between fork and exec.
            unsafe { cmd.pre_exec(affinity::child_setup(cpus.clone(), *nice)) };
        }
        let child = cmd.spawn()?;
        self.state.lock().pid = child.id();
        Ok(child)
    }

    fn supervise(self: &std::sync::Arc<Self>, mut child: tokio::process::Child) {
        let mut readers: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(tokio::spawn(self.clone().pump(out)));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(tokio::spawn(self.clone().pump(err)));
        }
        let this = self.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            // Let the readers drain what is left in the pipes; a grandchild that inherited
            // the pipe must not keep us waiting forever.
            for mut r in readers {
                if tokio::time::timeout(Duration::from_secs(2), &mut r).await.is_err() {
                    r.abort();
                }
            }
            let code = status.as_ref().ok().and_then(|s| s.code().or_else(|| s.signal().map(|sig| -sig)));
            {
                let mut st = this.state.lock();
                st.returncode = code;
                st.ended_at = Some(now_secs());
                st.status = if st.stopping {
                    Status::Stopped
                } else if code == Some(0) {
                    Status::Exited
                } else {
                    Status::Failed
                };
            }
            this.log(format!("[harness] プロセス終了 code={}", code.map_or("?".to_string(), |c| c.to_string())));
        });
    }

    async fn pump<R: AsyncRead + Unpin + Send + 'static>(self: std::sync::Arc<Self>, mut r: R) {
        let mut splitter = LineSplitter::default();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            match r.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for line in splitter.push(&chunk[..n]) {
                        self.log(line);
                    }
                }
            }
        }
        if let Some(line) = splitter.finish() {
            self.log(line);
        }
    }

    pub fn log(&self, line: impl Into<String>) {
        let mut st = self.state.lock();
        st.seq += 1;
        let seq = st.seq;
        st.logs.push_back((seq, line.into()));
        if st.logs.len() > LOG_CAP {
            st.logs.pop_front();
        }
    }

    #[allow(dead_code)]
    pub fn status(&self) -> Status {
        self.state.lock().status
    }

    #[allow(dead_code)]
    pub fn pid(&self) -> Option<u32> {
        self.state.lock().pid
    }

    #[allow(dead_code)]
    pub fn returncode(&self) -> Option<i32> {
        self.state.lock().returncode
    }

    pub fn alive(&self) -> bool {
        self.state.lock().status.is_alive()
    }

    /// Pid if the process is still alive.
    pub fn alive_pid(&self) -> Option<u32> {
        let st = self.state.lock();
        st.status.is_alive().then_some(st.pid).flatten()
    }

    /// Change the status of a live process (e.g. starting <-> running from health checks).
    /// Has no effect once the process ended.
    pub fn set_alive_status(&self, status: Status) {
        let mut st = self.state.lock();
        if st.status.is_alive() && status.is_alive() {
            st.status = status;
        }
    }

    /// SIGTERM the process group, wait up to 15 s, then SIGKILL.
    pub async fn stop(&self) {
        self.stop_within(Duration::from_secs(15)).await
    }

    pub async fn stop_within(&self, timeout: Duration) {
        let pid = {
            let mut st = self.state.lock();
            let Some(pid) = st.pid.filter(|_| st.status.is_alive()) else { return };
            st.stopping = true;
            pid as i32
        };
        // SAFETY: plain syscall; the child leads its own group (pgid == pid).
        if unsafe { libc::killpg(pid, libc::SIGTERM) } != 0 {
            return;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        while self.alive() {
            if tokio::time::Instant::now() >= deadline {
                // SAFETY: as above.
                unsafe { libc::killpg(pid, libc::SIGKILL) };
                for _ in 0..100 {
                    if !self.alive() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn tail(&self, since: u64, limit: usize) -> LogTail {
        let st = self.state.lock();
        let mut lines: Vec<LogLine> = st.logs.iter().rev().take_while(|(s, _)| *s > since).take(limit).map(|(s, l)| LogLine { seq: *s, text: l.clone() }).collect();
        lines.reverse();
        LogTail { lines, last: st.seq }
    }

    /// Base JSON shared by instances and job runs (`meta` entries are merged in).
    pub fn to_json(&self) -> Map<String, Value> {
        let st = self.state.lock();
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("name".into(), json!(self.name));
        m.insert("kind".into(), json!(self.kind));
        m.insert("cmd".into(), json!(self.cmd));
        m.insert("status".into(), json!(st.status.as_str()));
        m.insert("pid".into(), json!(st.pid));
        m.insert("returncode".into(), json!(st.returncode));
        m.insert("started_at".into(), json!(self.started_at));
        m.insert("ended_at".into(), json!(st.ended_at));
        for (k, v) in &self.meta {
            m.insert(k.clone(), v.clone());
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    async fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[test]
    fn splits_on_cr_and_lf() {
        let mut s = LineSplitter::default();
        let mut got = s.push(b"abc\rdef\n\n  \r\nghi");
        assert_eq!(got, vec!["abc", "def"]);
        got = s.push(b"jk\r\nlast");
        assert_eq!(got, vec!["ghijk"]);
        assert_eq!(s.finish().as_deref(), Some("last"));
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn lossy_utf8() {
        let mut s = LineSplitter::default();
        let got = s.push(&[b'a', 0xff, b'b', b'\n']);
        assert_eq!(got, vec!["a\u{fffd}b"]);
    }

    #[test]
    fn ring_buffer_and_tail() {
        let p = Proc::new("t", vec![], BTreeMap::new(), None, "instance", Map::new());
        for i in 0..LOG_CAP + 10 {
            p.log(format!("l{i}"));
        }
        let t = p.tail(0, usize::MAX);
        assert_eq!(t.lines.len(), LOG_CAP);
        assert_eq!(t.last, (LOG_CAP + 10) as u64);
        assert_eq!(t.lines[0].seq, 11);
        let t = p.tail(LOG_CAP as u64, 3);
        assert_eq!(t.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![(LOG_CAP + 8) as u64, (LOG_CAP + 9) as u64, (LOG_CAP + 10) as u64]);
    }

    #[tokio::test]
    async fn spawn_failure_is_failed_status() {
        let p = Proc::new("x", vec!["/nonexistent/definitely-not-here".into()], BTreeMap::new(), None, "instance", Map::new()).start().await;
        assert_eq!(p.status(), Status::Failed);
        assert!(p.tail(0, 100).lines.iter().any(|l| l.text.starts_with("[harness] 起動失敗:")));
    }

    #[tokio::test]
    async fn end_to_end_logs_and_stop() {
        let p = Proc::new("e2e", sh("echo a; echo b 1>&2; sleep 30"), BTreeMap::from([("FOO".to_string(), "bar".to_string())]), None, "instance", Map::new()).start().await;
        let pid = p.pid().expect("pid") as i32;
        assert!(wait_until(|| {
            let t = p.tail(0, 100);
            t.lines.iter().any(|l| l.text == "a") && t.lines.iter().any(|l| l.text == "b")
        })
        .await);
        assert!(p.alive());
        let started = std::time::Instant::now();
        p.stop().await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(wait_until(|| p.status() == Status::Stopped).await);
        assert!(p.returncode().is_some());
        // nothing but zombies may remain in the group
        let live_members = std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
            .filter(|s| {
                let rest = s.rsplit_once(')').map(|(_, r)| r.to_string()).unwrap_or_default();
                let f: Vec<&str> = rest.split_whitespace().collect();
                f.first() != Some(&"Z") && f.get(2).and_then(|g| g.parse::<i32>().ok()) == Some(pid)
            })
            .count();
        assert_eq!(live_members, 0);
        assert!(p.tail(0, 100).lines.iter().any(|l| l.text.starts_with("[harness] プロセス終了")));
    }

    #[tokio::test]
    async fn env_and_exit_code() {
        let p = Proc::new("env", sh("echo $FOO; exit 3"), BTreeMap::from([("FOO".to_string(), "bar".to_string())]), None, "job", Map::new()).start().await;
        assert!(wait_until(|| !p.alive()).await);
        assert_eq!(p.status(), Status::Failed);
        assert_eq!(p.returncode(), Some(3));
        assert!(p.tail(0, 100).lines.iter().any(|l| l.text == "bar"));
    }

    fn allowed_cpus() -> Vec<usize> {
        // SAFETY: sched_getaffinity fills a zeroed cpu_set_t.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set);
            (0..libc::CPU_SETSIZE as usize).filter(|&c| libc::CPU_ISSET(c, &set)).collect()
        }
    }

    #[tokio::test]
    async fn child_starts_pinned() {
        let cpu = allowed_cpus()[0];
        let p = Proc::new("pin", vec!["sleep".into(), "30".into()], BTreeMap::new(), None, "instance", Map::new()).pinned(vec![cpu], 5).start().await;
        let pid = p.pid().expect("pid");
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let line = status.lines().find(|l| l.starts_with("Cpus_allowed_list:")).unwrap();
        assert_eq!(line.split_once(':').unwrap().1.trim(), cpu.to_string());
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let nice: i32 = stat.rsplit_once(')').unwrap().1.split_whitespace().nth(16).unwrap().parse().unwrap();
        assert_eq!(nice, 5);
        p.stop().await;
        assert!(wait_until(|| p.status() == Status::Stopped).await);
    }
}
