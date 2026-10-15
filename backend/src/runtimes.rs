//! Runtime adapters: detection, command construction, health and metrics, setup jobs.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::config::{scripts_dir, Settings};

/// How long detected versions are reused (version probes spawn subprocesses and are slow).
const STATUS_TTL: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------ binaries
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub fn llama_server_bin(s: &Settings) -> Option<PathBuf> {
    let d = s.llama_cpp_dir();
    [d.join("build/bin/llama-server"), d.join("bin/llama-server")].into_iter().find(|p| p.is_file()).or_else(|| which("llama-server"))
}

pub fn vllm_bin(s: &Settings) -> Option<PathBuf> {
    let cand = s.vllm_venv().join("bin/vllm");
    if cand.is_file() {
        Some(cand)
    } else {
        which("vllm")
    }
}

/// Strata is a Python server (`serve/server.py`) living in its own checkout with a private venv.
pub fn strata_python(s: &Settings) -> Option<PathBuf> {
    let d = s.strata_dir();
    let py = d.join(".venv/bin/python");
    (py.is_file() && d.join("serve/server.py").is_file()).then_some(py)
}

/// A strata run config id: a plain `strata-<model>.json` file name, never a path.
pub fn valid_strata_config(id: &str) -> bool {
    id.starts_with("strata-") && id.ends_with(".json") && !id.contains(['/', '\\']) && !id.contains("..") && id.len() <= 120
}

/// Run configs the setup wizard wrote into the Strata checkout.
pub fn strata_configs(s: &Settings) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(s.strata_dir())
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| valid_strata_config(n)).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub fn ollama_bin() -> Option<PathBuf> {
    which("ollama")
}

// ------------------------------------------------------------------ versions
/// Run a command and return stdout + stderr (trimmed); `None` on spawn failure or timeout.
async fn run_capture(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(timeout, cmd.output()).await.ok()?.ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(text.trim().to_string())
}

static LLAMA_VERSION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"version:\s*(\S+(?:\s*\([0-9a-f]+\))?)").unwrap());
static CUDA_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"CUDA|ggml_cuda").unwrap());

pub fn parse_llama_version(out: &str) -> String {
    let ver = LLAMA_VERSION_RE
        .captures(out)
        .map(|c| c[1].to_string())
        .or_else(|| out.lines().last().map(str::to_string))
        .unwrap_or_else(|| "?".into());
    if CUDA_RE.is_match(out) {
        format!("{ver} CUDA")
    } else {
        ver
    }
}

async fn llama_version(bin: &Path) -> Option<String> {
    let out = run_capture(bin, &["--version"], Duration::from_secs(10)).await?;
    Some(parse_llama_version(&out))
}

async fn vllm_version(s: &Settings) -> Option<String> {
    let py = s.vllm_venv().join("bin/python");
    if !py.exists() {
        return None;
    }
    let out = run_capture(&py, &["-c", "import importlib.metadata as m; print(m.version('vllm'))"], Duration::from_secs(20)).await?;
    // A failing import prints a traceback; only accept something that looks like a version.
    out.lines().last().filter(|l| l.chars().next().is_some_and(|c| c.is_ascii_digit())).map(str::to_string)
}

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// Call the Ollama HTTP API; `None` if unreachable or the call failed.
pub async fn ollama_api(s: &Settings, path: &str, method: reqwest::Method, body: Option<Value>, timeout: Duration) -> Option<Value> {
    let url = format!("{}{}", s.ollama_url.trim_end_matches('/'), path);
    let mut req = HTTP.request(method, url).timeout(timeout);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.ok()?.error_for_status().ok()?;
    let bytes = resp.bytes().await.ok()?;
    if bytes.is_empty() {
        Some(Value::Object(Default::default()))
    } else {
        serde_json::from_slice(&bytes).ok()
    }
}

// -------------------------------------------------------------------- status
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct RuntimeStatus {
    pub id: &'static str,
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<bool>,
    pub install_job: &'static str,
    pub description: &'static str,
}

struct VersionCache {
    at: Instant,
    key: String,
    llama: Option<String>,
    vllm: Option<String>,
}

static CACHE: LazyLock<tokio::sync::Mutex<Option<VersionCache>>> = LazyLock::new(|| tokio::sync::Mutex::new(None));

/// Forget cached versions (call after an install job finished).
pub async fn invalidate_cache() {
    *CACHE.lock().await = None;
}

/// Status of every runtime. The slow subprocess version probes for llama.cpp and vLLM are
/// cached for 30 s; Ollama's version / `running` flag is a quick HTTP call and stays live.
pub async fn status(s: &Settings) -> Vec<RuntimeStatus> {
    let llama = llama_server_bin(s);
    let vllm = vllm_bin(s);
    let ollama = ollama_bin();
    let strata = strata_python(s);

    let key = format!("{}|{}", llama.as_deref().map_or(String::new(), |p| p.display().to_string()), vllm.as_deref().map_or(String::new(), |p| p.display().to_string()));
    let (llama_ver, vllm_ver) = {
        let mut cache = CACHE.lock().await;
        match cache.as_ref().filter(|c| c.key == key && c.at.elapsed() < STATUS_TTL) {
            Some(c) => (c.llama.clone(), c.vllm.clone()),
            None => {
                let (l, v) = tokio::join!(
                    async {
                        match &llama {
                            Some(b) => llama_version(b).await,
                            None => None,
                        }
                    },
                    async {
                        if vllm.is_some() {
                            vllm_version(s).await
                        } else {
                            None
                        }
                    }
                );
                *cache = Some(VersionCache { at: Instant::now(), key, llama: l.clone(), vllm: v.clone() });
                (l, v)
            }
        }
    };
    let ollama_ver = ollama_api(s, "/api/version", reqwest::Method::GET, None, Duration::from_secs(3)).await;

    vec![
        RuntimeStatus {
            id: "llama.cpp",
            installed: llama.is_some(),
            path: llama.map(|p| p.display().to_string()),
            version: llama_ver,
            running: None,
            install_job: "build-llama-cpp",
            description: "GGUF 推論の本命。CUDA (sm_86) ネイティブビルドで最速。",
        },
        RuntimeStatus {
            id: "vllm",
            installed: vllm.is_some(),
            path: vllm.map(|p| p.display().to_string()),
            version: vllm_ver,
            running: None,
            install_job: "install-vllm",
            description: "safetensors / AWQ / GPTQ 向けの高スループットサーバ。並列リクエストに強い。",
        },
        RuntimeStatus {
            id: "strata",
            installed: strata.is_some(),
            path: strata.map(|_| s.strata_dir().display().to_string()),
            version: None,
            running: None,
            install_job: "install-strata",
            description: "Strata: 大型 MoE (Qwen3.8-Flash-Next 等) を VRAM 12GB 級で動かす専用エンジン。インストール後に ./setup.sh でモデルを導入。",
        },
        RuntimeStatus {
            id: "ollama",
            installed: ollama.is_some(),
            path: ollama.map(|p| p.display().to_string()),
            version: ollama_ver.as_ref().and_then(|v| v.get("version")).and_then(Value::as_str).map(str::to_string),
            running: Some(ollama_ver.is_some()),
            install_job: "install-ollama",
            description: "手軽さ重視。モデル管理込み。中身は llama.cpp ベース。",
        },
    ]
}

// ------------------------------------------------------------------- command
/// llama-server flags that echo prompts / slot contents into its log or to disk.
const LLAMA_CONTENT_FLAGS: [&str; 3] = ["--verbose", "-v", "--slot-save-path"];
/// Strata flags that echo request traffic or write slot contents to disk.
const STRATA_CONTENT_FLAGS: [&str; 2] = ["--api-monitor", "--slot-save-path"];
/// vLLM flags that switch per-request logging on or off.
const VLLM_LOG_REQUEST_FLAGS: [&str; 2] = ["--disable-log-requests", "--enable-log-requests"];

/// The flag part of an argument: `--flag=value` and `--flag` both give `--flag`.
fn flag_name(arg: &str) -> &str {
    arg.split_once('=').map_or(arg, |(name, _)| name)
}

/// The first argument that is one of `names`, if any.
fn find_flag<'a>(args: &'a [String], names: &[&str]) -> Option<&'a str> {
    args.iter().map(String::as_str).find(|a| names.contains(&flag_name(a)))
}

/// Privacy mode: refuse user arguments that would make the runtime write request content to
/// its log or to disk. Rejecting (instead of silently dropping) keeps the profile honest.
fn forbid_content_flags(runtime: &str, args: &[String], names: &[&str]) -> anyhow::Result<()> {
    match find_flag(args, names) {
        Some(flag) => anyhow::bail!("プライバシーモードでは {runtime} の {flag} は使用できません (プロンプトや出力が書き出されるため)。引数から外すか、設定 privacy_mode を無効にしてください"),
        None => Ok(()),
    }
}

/// `(argv, env)` for starting a server. `env` holds only the additions to the inherited environment.
pub fn build_command(runtime: &str, model_path: &str, host: &str, port: u16, args: &[String], s: &Settings) -> anyhow::Result<(Vec<String>, BTreeMap<String, String>)> {
    let mut env: BTreeMap<String, String> = s.extra_env.clone();
    let exe = |p: PathBuf| p.display().to_string();
    match runtime {
        "llama.cpp" => {
            let Some(bin) = llama_server_bin(s) else {
                anyhow::bail!("llama-server が見つかりません。Runtimes から llama.cpp をビルドしてください");
            };
            if s.privacy_mode {
                forbid_content_flags("llama.cpp", args, &LLAMA_CONTENT_FLAGS)?;
            }
            let mut cmd = vec![exe(bin), "-m".into(), model_path.to_string(), "--host".into(), host.to_string(), "--port".into(), port.to_string()];
            cmd.extend(args.iter().cloned());
            Ok((cmd, env))
        }
        "vllm" => {
            let Some(bin) = vllm_bin(s) else {
                anyhow::bail!("vLLM が見つかりません。Runtimes からインストールしてください");
            };
            env.entry("VLLM_USE_V1".into()).or_insert_with(|| "1".into());
            let mut cmd = vec![exe(bin), "serve".into(), model_path.to_string(), "--host".into(), host.to_string(), "--port".into(), port.to_string()];
            // Privacy mode: no per-request log lines (they include prompt text). An explicit
            // choice in the user's args, either way, is respected.
            if s.privacy_mode && find_flag(args, &VLLM_LOG_REQUEST_FLAGS).is_none() {
                cmd.push("--disable-log-requests".into());
            }
            cmd.extend(args.iter().cloned());
            Ok((cmd, env))
        }
        "strata" => {
            let Some(py) = strata_python(s) else {
                anyhow::bail!("Strata が見つかりません。Runtimes からインストールし、strata ディレクトリで ./setup.sh を実行してください");
            };
            if !valid_strata_config(model_path_name(model_path)) {
                anyhow::bail!("Strata のモデルは strata-<name>.json の設定ファイルを指定してください");
            }
            let cfg = s.strata_dir().join(model_path_name(model_path));
            if !cfg.is_file() {
                anyhow::bail!("設定ファイル {} がありません。strata ディレクトリで ./setup.sh を実行してください", cfg.display());
            }
            if s.privacy_mode {
                forbid_content_flags("Strata", args, &STRATA_CONTENT_FLAGS)?;
            }
            let mut cmd = vec![exe(py), "-m".into(), "serve.server".into(), "--engine".into(), "strata".into(), "--config".into(), cfg.display().to_string(), "--host".into(), host.to_string(), "--port".into(), port.to_string()];
            cmd.extend(args.iter().cloned());
            Ok((cmd, env))
        }
        "ollama" => {
            let Some(bin) = ollama_bin() else {
                anyhow::bail!("ollama が見つかりません");
            };
            for (k, v) in [
                ("OLLAMA_HOST", format!("{host}:{port}")),
                ("OLLAMA_FLASH_ATTENTION", "1".into()),
                ("OLLAMA_KV_CACHE_TYPE", "q8_0".into()),
                ("OLLAMA_NUM_PARALLEL", "2".into()),
                ("OLLAMA_KEEP_ALIVE", "30m".into()),
            ] {
                env.insert(k.into(), v);
            }
            let mut cmd = vec![exe(bin), "serve".into()];
            cmd.extend(args.iter().cloned());
            Ok((cmd, env))
        }
        other => anyhow::bail!("unknown runtime {other}"),
    }
}

fn model_path_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Prometheus endpoint of a runtime (Strata serves JSON on /metrics, vLLM-named Prometheus text on /metrics/prometheus).
pub fn metrics_path(runtime: &str) -> &'static str {
    if runtime == "strata" { "/metrics/prometheus" } else { "/metrics" }
}

pub fn health_path(runtime: &str) -> &'static str {
    match runtime {
        "ollama" => "/",
        _ => "/health",
    }
}

// ------------------------------------------------------------------- metrics
static METRIC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([a-zA-Z_:]+)(?:\{[^}]*\})?\s+([0-9.eE+-]+|NaN)$").unwrap());

fn metric_key(name: &str) -> Option<&'static str> {
    Some(match name {
        "llamacpp:prompt_tokens_seconds" => "prompt_tps",
        "llamacpp:predicted_tokens_seconds" => "gen_tps",
        "llamacpp:requests_processing" | "vllm:num_requests_running" => "processing",
        "llamacpp:requests_deferred" | "vllm:num_requests_waiting" => "deferred",
        "llamacpp:tokens_predicted_total" | "vllm:generation_tokens_total" => "tokens_total",
        "llamacpp:kv_cache_usage_ratio" | "vllm:gpu_cache_usage_perc" | "vllm:kv_cache_usage_perc" => "kv_usage",
        _ => return None,
    })
}

/// Parse Prometheus text, keeping the metrics the UI shows; labelled series are summed.
pub fn parse_metrics(text: &str) -> BTreeMap<String, f64> {
    let mut out: BTreeMap<String, f64> = BTreeMap::new();
    for line in text.lines() {
        let Some(c) = METRIC_RE.captures(line) else { continue };
        let Some(key) = metric_key(&c[1]) else { continue };
        if let Ok(v) = c[2].parse::<f64>() {
            if v.is_finite() {
                *out.entry(key.to_string()).or_insert(0.0) += v;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------- jobs
pub struct JobDef {
    pub id: &'static str,
    pub title: &'static str,
    /// Relative to `scripts/`.
    pub script: &'static str,
    pub sudo: bool,
}

pub const JOBS: [JobDef; 8] = [
    JobDef { id: "setup", title: "WSL 基本セットアップ (apt / CUDA Toolkit / uv)", script: "wsl/setup.sh", sudo: true },
    JobDef { id: "build-llama-cpp", title: "llama.cpp を CUDA でビルド / 更新", script: "wsl/build-llama-cpp.sh", sudo: false },
    JobDef { id: "install-vllm", title: "vLLM を専用 venv にインストール / 更新", script: "wsl/install-vllm.sh", sudo: false },
    JobDef { id: "install-strata", title: "Strata を取得 / 更新 (git clone + venv)", script: "wsl/install-strata.sh", sudo: false },
    JobDef { id: "update-strata", title: "Strata を更新 (./update.sh のみ・モデルは取得しない)", script: "wsl/update-strata.sh", sudo: false },
    JobDef { id: "bench-strata", title: "Strata をベンチマーク (./bench.sh・5〜10 分・GPU を使用)", script: "wsl/bench-strata.sh", sudo: false },
    JobDef { id: "install-ollama", title: "Ollama をインストール / 更新", script: "wsl/install-ollama.sh", sudo: true },
    JobDef { id: "update-harness", title: "ハーネス本体を更新 (git pull --ff-only → ./harness.sh install)", script: "wsl/update-harness.sh", sudo: false },
];

pub fn job(id: &str) -> Option<&'static JobDef> {
    JOBS.iter().find(|j| j.id == id)
}

/// `(argv, env additions)` running a setup script.
pub fn job_command(job_id: &str, s: &Settings) -> anyhow::Result<(Vec<String>, BTreeMap<String, String>)> {
    let Some(j) = job(job_id) else { anyhow::bail!("unknown job") };
    let env = BTreeMap::from([
        ("LLAMA_CPP_DIR".to_string(), s.llama_cpp_dir().display().to_string()),
        ("VLLM_VENV".to_string(), s.vllm_venv().display().to_string()),
        ("STRATA_DIR".to_string(), s.strata_dir().display().to_string()),
        ("PATH".to_string(), format!("{}:/usr/local/cuda/bin", std::env::var("PATH").unwrap_or_default())),
    ]);
    Ok((vec!["bash".into(), scripts_dir().join(j.script).display().to_string()], env))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strata_config_ids_are_plain_file_names() {
        assert!(valid_strata_config("strata-Q2_0.json"));
        for bad in ["../strata-x.json", "strata-/x.json", "strata-..json/x", "model.json", "strata-x.txt", "/etc/strata-x.json", "strata-a\\b.json"] {
            assert!(!valid_strata_config(bad), "{bad}");
        }
    }

    #[test]
    fn strata_command_uses_private_venv_and_config() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings { strata_dir: dir.path().display().to_string(), ..Default::default() };
        assert!(build_command("strata", "strata-a.json", "127.0.0.1", 8090, &[], &s).is_err(), "not installed");
        std::fs::create_dir_all(dir.path().join(".venv/bin")).unwrap();
        std::fs::create_dir_all(dir.path().join("serve")).unwrap();
        std::fs::write(dir.path().join(".venv/bin/python"), "").unwrap();
        std::fs::write(dir.path().join("serve/server.py"), "").unwrap();
        assert!(build_command("strata", "strata-a.json", "127.0.0.1", 8090, &[], &s).is_err(), "config missing");
        std::fs::write(dir.path().join("strata-a.json"), "{}").unwrap();
        let (cmd, _) = build_command("strata", "strata-a.json", "127.0.0.1", 8090, &["--lazy".into()], &s).unwrap();
        assert!(cmd[0].ends_with(".venv/bin/python"));
        assert_eq!(&cmd[1..5], ["-m", "serve.server", "--engine", "strata"]);
        assert!(cmd.contains(&"--lazy".to_string()) && cmd.contains(&"8090".to_string()));
        assert_eq!(strata_configs(&s), vec!["strata-a.json".to_string()]);
        assert_eq!(metrics_path("strata"), "/metrics/prometheus");
    }

    #[test]
    fn metrics_sum_labelled_series() {
        let text = "\
# HELP llamacpp:prompt_tokens_seconds x
# TYPE llamacpp:prompt_tokens_seconds gauge
llamacpp:prompt_tokens_seconds 123.5
llamacpp:predicted_tokens_seconds 45.25
llamacpp:requests_processing 1
llamacpp:kv_cache_usage_ratio 0.25
vllm:num_requests_running{model_name=\"a\",engine=\"0\"} 2
vllm:num_requests_running{model_name=\"b\",engine=\"1\"} 3
vllm:generation_tokens_total{model_name=\"a\"} 1.5e3
vllm:kv_cache_usage_perc{engine=\"0\"} 0.5
vllm:gpu_cache_usage_perc{engine=\"0\"} NaN
unrelated_metric 99
";
        let m = parse_metrics(text);
        assert_eq!(m["prompt_tps"], 123.5);
        assert_eq!(m["gen_tps"], 45.25);
        assert_eq!(m["processing"], 1.0 + 5.0);
        assert_eq!(m["tokens_total"], 1500.0);
        assert_eq!(m["kv_usage"], 0.75);
        assert!(!m.contains_key("deferred"));
        assert!(!m.contains_key("unrelated_metric"));
    }

    #[test]
    fn llama_version_parse() {
        assert_eq!(parse_llama_version("version: 4567 (abc123f)\nbuilt with gcc"), "4567 (abc123f)");
        assert_eq!(parse_llama_version("ggml_cuda_init: found 1 CUDA devices\nversion: 1 (deadbee)"), "1 (deadbee) CUDA");
        assert_eq!(parse_llama_version(""), "?");
    }

    #[test]
    fn health_paths() {
        assert_eq!(health_path("llama.cpp"), "/health");
        assert_eq!(health_path("vllm"), "/health");
        assert_eq!(health_path("ollama"), "/");
    }

    #[test]
    fn jobs_table() {
        assert_eq!(JOBS.len(), 8);
        assert_eq!(JOBS.iter().filter(|j| j.sudo).map(|j| j.id).collect::<Vec<_>>(), vec!["setup", "install-ollama"]);
        // Strata maintenance runs without sudo and as its own scripts
        for id in ["update-strata", "bench-strata"] {
            let j = job(id).unwrap_or_else(|| panic!("job {id}"));
            assert!(!j.sudo && j.script.starts_with("wsl/") && j.script.ends_with(".sh"), "{id}");
            assert!(scripts_dir().join(j.script).is_file(), "script for {id} exists");
        }
        let (cmd, env) = job_command("build-llama-cpp", &Settings::default()).unwrap();
        assert_eq!(cmd[0], "bash");
        assert!(cmd[1].ends_with("scripts/wsl/build-llama-cpp.sh"));
        assert!(env["PATH"].ends_with(":/usr/local/cuda/bin"));
        assert!(job_command("nope", &Settings::default()).is_err());
    }

    #[test]
    fn command_construction() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("build/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("llama-server"), "#!/bin/sh\n").unwrap();
        let mut s = Settings { llama_cpp_dir: dir.path().display().to_string(), vllm_venv: dir.path().join("nov").display().to_string(), ..Default::default() };
        s.extra_env.insert("X".into(), "1".into());
        let (cmd, env) = build_command("llama.cpp", "/m/a.gguf", "127.0.0.1", 8080, &["-c".into(), "4096".into()], &s).unwrap();
        assert!(cmd[0].ends_with("build/bin/llama-server"));
        assert_eq!(cmd[1..], ["-m", "/m/a.gguf", "--host", "127.0.0.1", "--port", "8080", "-c", "4096"]);
        assert_eq!(env["X"], "1");
        assert!(build_command("bogus", "", "h", 1, &[], &s).is_err());
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Fake llama-server, vLLM venv and Strata checkout under one temp dir.
    fn fake_install(dir: &Path) -> Settings {
        std::fs::create_dir_all(dir.join("llama/build/bin")).unwrap();
        std::fs::write(dir.join("llama/build/bin/llama-server"), "").unwrap();
        std::fs::create_dir_all(dir.join("venv/bin")).unwrap();
        std::fs::write(dir.join("venv/bin/vllm"), "").unwrap();
        std::fs::create_dir_all(dir.join("strata/.venv/bin")).unwrap();
        std::fs::create_dir_all(dir.join("strata/serve")).unwrap();
        std::fs::write(dir.join("strata/.venv/bin/python"), "").unwrap();
        std::fs::write(dir.join("strata/serve/server.py"), "").unwrap();
        std::fs::write(dir.join("strata/strata-a.json"), "{}").unwrap();
        Settings {
            llama_cpp_dir: dir.join("llama").display().to_string(),
            vllm_venv: dir.join("venv").display().to_string(),
            strata_dir: dir.join("strata").display().to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn privacy_mode_is_on_by_default_and_blocks_llama_content_flags() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = fake_install(dir.path());
        assert!(s.privacy_mode);
        for bad in ["-v", "--verbose", "--slot-save-path", "--slot-save-path=/tmp/x"] {
            assert!(build_command("llama.cpp", "/m/a.gguf", "127.0.0.1", 8080, &strs(&["-c", "512", bad]), &s).is_err(), "{bad} must be refused");
        }
        // unrelated flags that only share a prefix are fine
        let (cmd, _) = build_command("llama.cpp", "/m/a.gguf", "127.0.0.1", 8080, &strs(&["--verbose-prompt", "--log-disable"]), &s).unwrap();
        assert!(cmd.contains(&"--verbose-prompt".to_string()));
        // with privacy off the user is in charge
        s.privacy_mode = false;
        assert!(build_command("llama.cpp", "/m/a.gguf", "127.0.0.1", 8080, &strs(&["-v"]), &s).is_ok());
        // llama.cpp never gets --disable-log-requests (vLLM only)
        s.privacy_mode = true;
        let (cmd, _) = build_command("llama.cpp", "/m/a.gguf", "127.0.0.1", 8080, &[], &s).unwrap();
        assert!(!cmd.iter().any(|a| a.starts_with("--disable-log") || a == "-v" || a == "--verbose" || a.starts_with("--slot-save")));
    }

    #[test]
    fn vllm_gets_disable_log_requests_in_privacy_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = fake_install(dir.path());
        let count = |cmd: &[String]| cmd.iter().filter(|a| *a == "--disable-log-requests").count();

        let (cmd, _) = build_command("vllm", "/m/org/model", "127.0.0.1", 8000, &strs(&["--max-model-len", "8192"]), &s).unwrap();
        assert_eq!(count(&cmd), 1, "added in privacy mode: {cmd:?}");
        assert!(cmd.contains(&"8192".to_string()));

        // already requested by the user: not duplicated
        let (cmd, _) = build_command("vllm", "/m/org/model", "127.0.0.1", 8000, &strs(&["--disable-log-requests"]), &s).unwrap();
        assert_eq!(count(&cmd), 1);
        // the user explicitly re-enabled request logs: respected
        let (cmd, _) = build_command("vllm", "/m/org/model", "127.0.0.1", 8000, &strs(&["--enable-log-requests"]), &s).unwrap();
        assert_eq!(count(&cmd), 0);
        // privacy off: nothing added
        s.privacy_mode = false;
        let (cmd, _) = build_command("vllm", "/m/org/model", "127.0.0.1", 8000, &[], &s).unwrap();
        assert_eq!(count(&cmd), 0);
    }

    #[test]
    fn strata_refuses_api_monitor_and_slot_save_in_privacy_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = fake_install(dir.path());
        for bad in ["--api-monitor", "--slot-save-path=/x"] {
            assert!(build_command("strata", "strata-a.json", "127.0.0.1", 8090, &strs(&[bad]), &s).is_err(), "{bad}");
        }
        let (cmd, _) = build_command("strata", "strata-a.json", "127.0.0.1", 8090, &strs(&["--lazy"]), &s).unwrap();
        assert!(!cmd.iter().any(|a| a == "--api-monitor" || a.starts_with("--slot-save-path")));
        s.privacy_mode = false;
        assert!(build_command("strata", "strata-a.json", "127.0.0.1", 8090, &strs(&["--api-monitor"]), &s).is_ok());
    }
}
