//! STUB — to be implemented. Minimal GGUF header reader (metadata KV only).
#![allow(dead_code, unused_variables)]

use std::path::Path;

use crate::types::GgufInfo;

/// Read the header of `path` and summarise it. Never panics; errors go to `GgufInfo::error`.
pub fn read_info(path: &Path) -> GgufInfo {
    GgufInfo::default()
}
