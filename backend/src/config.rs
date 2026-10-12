//! Persistent settings and paths.
//!
//! Everything lives under `HARNESS_HOME` (default `~/.llm-harness`). Models default to
//! `~/models` on the WSL ext4 filesystem: keeping weights off `/mnt/c` avoids the 9P
//! bridge, which is an order of magnitude slower for multi-GB reads.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Context;
use parking_lot::RwLock;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::mode::ModeSettings;

pub static HOME: LazyLock<PathBuf> = LazyLock::new(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root")));

pub static HARNESS_HOME: LazyLock<PathBuf> = LazyLock::new(|| {
    std::env::var_os("HARNESS_HOME").map(PathBuf::from).unwrap_or_else(|| HOME.join(".llm-harness"))
});

/// Repository root (for `scripts/`). Overridable for installed binaries.
pub static REPO_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    if let Some(p) = std::env::var_os("HARNESS_REPO") {
        return PathBuf::from(p);
    }
    // binary lives in <repo>/backend/target/{debug,release}/
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().find(|a| a.join("scripts/wsl").is_dir()).map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."))
});

pub fn scripts_dir() -> PathBuf {
    REPO_ROOT.join("scripts")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub models_dir: String,
    pub hf_token: String,
    pub hf_endpoint: String,
    /// llama.cpp checkout built by scripts/wsl/build-llama-cpp.sh
    pub llama_cpp_dir: String,
    pub vllm_venv: String,
    /// Strata checkout (git clone + its own .venv); model configs are `strata-<model>.json` inside.
    pub strata_dir: String,
    pub ollama_url: String,
    /// Used in full mode; shared mode uses `mode.shared.*`.
    pub max_concurrent_downloads: usize,
    /// Parallel HTTP range connections per file in full mode (HF's CDN caps per-connection speed).
    pub connections_per_file: usize,
    /// Bind address for spawned model servers. 127.0.0.1 is still reachable from Windows via localhost.
    pub default_host: String,
    pub port_range_start: u16,
    pub extra_env: std::collections::BTreeMap<String, String>,
    pub mode: ModeSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            models_dir: HOME.join("models").to_string_lossy().into(),
            hf_token: std::env::var("HF_TOKEN").unwrap_or_default(),
            hf_endpoint: "https://huggingface.co".into(),
            llama_cpp_dir: HARNESS_HOME.join("llama.cpp").to_string_lossy().into(),
            vllm_venv: HARNESS_HOME.join("venvs/vllm").to_string_lossy().into(),
            strata_dir: HARNESS_HOME.join("strata").to_string_lossy().into(),
            ollama_url: "http://127.0.0.1:11434".into(),
            max_concurrent_downloads: 2,
            connections_per_file: 8,
            default_host: "127.0.0.1".into(),
            port_range_start: 8080,
            extra_env: Default::default(),
            mode: ModeSettings::default(),
        }
    }
}

impl Settings {
    pub fn models_dir(&self) -> PathBuf {
        expand(&self.models_dir)
    }
    pub fn llama_cpp_dir(&self) -> PathBuf {
        expand(&self.llama_cpp_dir)
    }
    pub fn vllm_venv(&self) -> PathBuf {
        expand(&self.vllm_venv)
    }
    pub fn strata_dir(&self) -> PathBuf {
        expand(&self.strata_dir)
    }
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => HOME.join(rest),
        None if p == "~" => HOME.clone(),
        None => PathBuf::from(p),
    }
}

pub struct SettingsStore {
    inner: RwLock<Settings>,
    path: PathBuf,
}

impl SettingsStore {
    pub fn load() -> anyhow::Result<Self> {
        std::fs::create_dir_all(&*HARNESS_HOME).with_context(|| format!("create {}", HARNESS_HOME.display()))?;
        let path = HARNESS_HOME.join("settings.json");
        let settings = read_json_file::<Settings>(&path).unwrap_or_default();
        let _ = std::fs::create_dir_all(settings.models_dir());
        Ok(Self { inner: RwLock::new(settings), path })
    }

    /// Cheap clone of the current settings.
    pub fn get(&self) -> Settings {
        self.inner.read().clone()
    }

    /// Merge a partial JSON object into the settings (unknown keys are ignored) and persist.
    pub fn update(&self, patch: &serde_json::Value) -> anyhow::Result<Settings> {
        let mut guard = self.inner.write();
        let mut current = serde_json::to_value(&*guard)?;
        merge_json(&mut current, patch);
        let next: Settings = serde_json::from_value(current).context("invalid settings")?;
        write_json_file(&self.path, &next)?;
        let _ = std::fs::create_dir_all(next.models_dir());
        *guard = next.clone();
        Ok(next)
    }

    /// Mutate in place and persist.
    pub fn modify(&self, f: impl FnOnce(&mut Settings)) -> anyhow::Result<Settings> {
        let mut guard = self.inner.write();
        let mut next = guard.clone();
        f(&mut next);
        write_json_file(&self.path, &next)?;
        *guard = next.clone();
        Ok(next)
    }
}

/// Recursive merge: objects are merged key by key, everything else is replaced.
pub fn merge_json(base: &mut serde_json::Value, patch: &serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(b), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                if let Some(slot) = b.get_mut(k) {
                    merge_json(slot, v);
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

pub fn read_json_file<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Atomic write (tmp + rename) so a crash never leaves a truncated state file.
pub fn write_json_file<T: Serialize + ?Sized>(path: &Path, value: &T) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// State file under HARNESS_HOME, e.g. `read_json("downloads.json")`.
pub fn read_json<T: DeserializeOwned>(name: &str) -> Option<T> {
    read_json_file(&HARNESS_HOME.join(name))
}

pub fn write_json<T: Serialize + ?Sized>(name: &str, value: &T) -> anyhow::Result<()> {
    write_json_file(&HARNESS_HOME.join(name), value)
}
