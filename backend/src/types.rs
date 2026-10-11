//! Data types shared between modules. Field names match the JSON the frontend expects.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub memory_total_mb: f64,
    pub memory_used_mb: f64,
    pub memory_free_mb: f64,
    pub util_gpu: f64,
    pub util_mem: f64,
    pub temperature: f64,
    pub power_draw: Option<f64>,
    pub power_limit: Option<f64>,
    /// Board default power limit (W), used to compute the shared-mode limit.
    pub power_default_limit: Option<f64>,
    pub clock_sm: f64,
    pub clock_sm_max: f64,
    pub pcie_gen: String,
    pub pcie_width: String,
    pub driver: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CpuTopology {
    pub model: String,
    pub physical_cores: usize,
    pub logical_cores: usize,
    /// One entry per physical core: the logical CPU ids (SMT siblings) belonging to it.
    pub cores: Vec<Vec<usize>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub ok: bool,
    /// "info" | "warn" | "error"
    pub level: String,
    pub message: String,
}

impl Check {
    pub fn new(id: &str, ok: bool, level: &str, message: impl Into<String>) -> Self {
        Self { id: id.into(), ok, level: level.into(), message: message.into() }
    }
}

/// Summary of a GGUF header (see `gguf::summarize`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GgufInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_count_kv: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_length: Option<u64>,
    pub expert_count: Option<u64>,
    pub expert_used_count: Option<u64>,
    #[serde(default)]
    pub has_chat_template: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LocalModel {
    /// Path relative to models_dir (first shard for split GGUF, directory for safetensors).
    pub id: String,
    /// "gguf" | "safetensors"
    pub format: String,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub files: usize,
    pub mmproj: Option<String>,
    pub runtimes: Vec<String>,
    pub info: GgufInfo,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TuneEstimate {
    pub model_bytes: u64,
    pub kv_bytes: u64,
    pub vram_free: u64,
    pub vram_total: u64,
    /// VRAM held back for the desktop by the current mode.
    pub vram_reserve: u64,
    pub fits: bool,
    pub ctx: u64,
    pub native_ctx: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TuneResult {
    pub args: Vec<String>,
    pub notes: Vec<String>,
    pub estimate: TuneEstimate,
}
