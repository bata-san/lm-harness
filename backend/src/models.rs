//! Local model inventory: GGUF files (llama.cpp / ollama) and HF safetensors dirs (vLLM).
//! Routes: GET /api/models, DELETE /api/models/{*id}

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{bail, Context, Result};
use axum::extract::{Path as UrlPath, State};
use axum::routing::get;
use axum::{Json, Router};
use regex::Regex;

use crate::error::{ok, ApiError, ApiResult};
use crate::gguf;
use crate::state::AppState;
use crate::types::{GgufInfo, LocalModel};

/// `<base>-00001-of-00003.gguf`
static SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<base>.+)-(?P<idx>\d{5})-of-(?P<total>\d{5})\.gguf$").expect("valid regex")
});

/// `(base, total)` for a split shard name.
fn split_key(name: &str) -> Option<(String, String)> {
    let caps = SPLIT.captures(name)?;
    Some((caps["base"].to_owned(), caps["total"].to_owned()))
}

struct FileEntry {
    name: String,
    path: PathBuf,
    size: u64,
}

/// The regular files directly inside one directory.
struct DirListing {
    dir: PathBuf,
    /// Sorted by name.
    files: Vec<FileEntry>,
}

/// Collects every directory under `dir`. Hidden directories and symlinked directories are
/// not descended into; `*.part` download leftovers are ignored.
fn walk(dir: &Path, out: &mut Vec<DirListing>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut listing = DirListing {
        dir: dir.to_path_buf(),
        files: Vec::new(),
    };
    let mut subdirs = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        // file_type() does not follow symlinks.
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if !name.starts_with('.') {
                subdirs.push(path);
            }
            continue;
        }
        if name.ends_with(".part") {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.is_file() {
                listing.files.push(FileEntry {
                    name,
                    path,
                    size: meta.len(),
                });
            }
        }
    }
    listing.files.sort_by(|a, b| a.name.cmp(&b.name));
    out.push(listing);
    for sub in subdirs {
        walk(&sub, out);
    }
}

fn relative(models_dir: &Path, path: &Path) -> String {
    path.strip_prefix(models_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Lists every local model under `models_dir`, sorted by id.
pub fn scan(models_dir: &Path) -> Vec<LocalModel> {
    let mut listings = Vec::new();
    walk(models_dir, &mut listings);

    let mut out = Vec::new();
    for listing in &listings {
        // Split shards are grouped by (base, total) within a directory.
        let mut splits: BTreeMap<(String, String), Vec<&FileEntry>> = BTreeMap::new();
        let mut singles: Vec<Vec<&FileEntry>> = Vec::new();
        let mut mmproj: Option<&FileEntry> = None;
        for f in listing.files.iter().filter(|f| f.name.ends_with(".gguf")) {
            if f.name.to_lowercase().contains("mmproj") {
                // Files are name-sorted, so the first one wins.
                mmproj.get_or_insert(f);
            } else if let Some(key) = split_key(&f.name) {
                splits.entry(key).or_default().push(f);
            } else {
                singles.push(vec![f]);
            }
        }
        let mmproj = mmproj.map(|f| f.path.to_string_lossy().into_owned());
        let groups: Vec<Vec<&FileEntry>> =
            singles.into_iter().chain(splits.into_values()).collect();
        for parts in &groups {
            out.push(gguf_entry(models_dir, parts, mmproj.clone()));
        }

        // The models root itself is not an entry: its id would be "." and cannot be deleted.
        let weights: Vec<&FileEntry> = listing
            .files
            .iter()
            .filter(|f| f.name.ends_with(".safetensors"))
            .collect();
        let has_config = listing.files.iter().any(|f| f.name == "config.json");
        if has_config && !weights.is_empty() && listing.dir != models_dir {
            out.push(safetensors_entry(models_dir, &listing.dir, &weights));
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn gguf_entry(models_dir: &Path, parts: &[&FileEntry], mmproj: Option<String>) -> LocalModel {
    let first = parts[0];
    LocalModel {
        id: relative(models_dir, &first.path),
        format: "gguf".into(),
        path: first.path.to_string_lossy().into_owned(),
        name: first.name.clone(),
        size: parts.iter().map(|p| p.size).sum(),
        files: parts.len(),
        mmproj,
        runtimes: vec!["llama.cpp".into(), "ollama".into()],
        info: gguf::read_info(&first.path),
    }
}

fn safetensors_entry(models_dir: &Path, dir: &Path, weights: &[&FileEntry]) -> LocalModel {
    LocalModel {
        id: relative(models_dir, dir),
        format: "safetensors".into(),
        path: dir.to_string_lossy().into_owned(),
        name: dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size: weights.iter().map(|w| w.size).sum(),
        files: weights.len(),
        mmproj: None,
        runtimes: vec!["vllm".into()],
        info: GgufInfo::default(),
    }
}

/// Resolve a model id (relative path) inside models_dir, rejecting path traversal.
///
/// The id must be a non-empty relative path made of normal components. Any existing
/// prefix is canonicalised, so a symlink pointing outside the models directory is refused.
/// Ids that do not exist yet are allowed (the result is then lexically inside the root).
pub fn resolve(models_dir: &Path, id: &str) -> Result<PathBuf> {
    let rel = Path::new(id);
    let mut normal = 0usize;
    for component in rel.components() {
        match component {
            Component::Normal(_) => normal += 1,
            Component::CurDir => {}
            _ => bail!("invalid model id {id:?}"),
        }
    }
    if normal == 0 {
        bail!("invalid model id {id:?}");
    }

    let root = models_dir
        .canonicalize()
        .context("models directory is not accessible")?;
    let joined = root.join(rel);
    // Deepest existing ancestor (the root itself exists, so this always terminates).
    let mut ancestor = joined.as_path();
    while std::fs::symlink_metadata(ancestor).is_err() {
        match ancestor.parent() {
            Some(parent) => ancestor = parent,
            None => bail!("invalid model id {id:?}"),
        }
    }
    let real = ancestor
        .canonicalize()
        .with_context(|| format!("invalid model id {id:?}"))?;
    if !real.starts_with(&root) {
        bail!("path outside models directory");
    }
    let tail = joined.strip_prefix(ancestor).context("invalid model id")?;
    // An empty tail must not be joined: "file/" is not a valid path for an existing file.
    let full = if tail.as_os_str().is_empty() {
        real
    } else {
        real.join(tail)
    };
    if full == root {
        bail!("invalid model id {id:?}");
    }
    Ok(full)
}

/// Deletes a model: a directory tree, a whole split GGUF set, or a single file.
/// Missing models are not an error.
pub fn delete(models_dir: &Path, id: &str) -> Result<()> {
    let path = resolve(models_dir, id)?;
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if meta.is_dir() {
        return std::fs::remove_dir_all(&path)
            .with_context(|| format!("removing {}", path.display()));
    }
    let targets = match path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(split_key)
    {
        Some(key) => split_set(&path, &key)?,
        None => vec![path],
    };
    for target in &targets {
        match std::fs::remove_file(target) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Every shard in `path`'s directory that belongs to the same split set.
fn split_set(path: &Path, key: &(String, String)) -> Result<Vec<PathBuf>> {
    let mut out = vec![path.to_path_buf()];
    let Some(dir) = path.parent() else {
        return Ok(out);
    };
    for entry in std::fs::read_dir(dir)?.flatten() {
        let p = entry.path();
        if p == path || !entry.file_type().is_ok_and(|t| !t.is_dir()) {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            if split_key(name).as_ref() == Some(key) {
                out.push(p);
            }
        }
    }
    Ok(out)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/models", get(list_models))
        .route("/api/models/{*id}", axum::routing::delete(delete_model))
}

async fn list_models(State(state): State<AppState>) -> ApiResult<Vec<LocalModel>> {
    let dir = state.settings.get().models_dir();
    let models = tokio::task::spawn_blocking(move || scan(&dir))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(models))
}

async fn delete_model(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<serde_json::Value> {
    let dir = state.settings.get().models_dir();
    resolve(&dir, &id).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    tokio::task::spawn_blocking(move || delete(&dir, &id))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
    ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn scan_groups_split_shards_under_the_first_shard() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("org/Model/Model-Q4-00002-of-00002.gguf"), b"xx");
        touch(&root.join("org/Model/Model-Q4-00001-of-00002.gguf"), b"xxx");
        touch(
            &root.join("org/Model/Model-Q4-00002-of-00002.gguf.part"),
            b"partial",
        );

        let models = scan(root);
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "org/Model/Model-Q4-00001-of-00002.gguf");
        assert_eq!(m.format, "gguf");
        assert_eq!(m.size, 5);
        assert_eq!(m.files, 2);
        assert_eq!(m.mmproj, None);
        assert_eq!(
            m.runtimes,
            vec!["llama.cpp".to_string(), "ollama".to_string()]
        );
    }

    #[test]
    fn scan_attaches_mmproj_to_gguf_in_same_directory_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("vision/llava-7b-Q4.gguf"), b"x");
        touch(&root.join("vision/mmproj-f16.gguf"), b"x");
        touch(&root.join("vision/zz-MMPROJ-q8.gguf"), b"x");
        touch(&root.join("text/plain.gguf"), b"x");

        let models = scan(root);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["text/plain.gguf", "vision/llava-7b-Q4.gguf"]);
        assert_eq!(models[0].mmproj, None);
        let mmproj = models[1].mmproj.as_deref().unwrap();
        assert!(mmproj.ends_with("vision/mmproj-f16.gguf"), "{mmproj}");
    }

    #[test]
    fn scan_finds_safetensors_dirs_and_skips_hidden_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("hf/Qwen-7B/config.json"), b"{}");
        touch(
            &root.join("hf/Qwen-7B/model-00001-of-00002.safetensors"),
            b"aaaa",
        );
        touch(
            &root.join("hf/Qwen-7B/model-00002-of-00002.safetensors"),
            b"bb",
        );
        touch(&root.join("hf/Qwen-7B/model.safetensors.part"), b"zzzzzzzz");
        touch(&root.join(".cache/hidden.gguf"), b"x");
        touch(&root.join(".cache/hf/config.json"), b"{}");
        touch(&root.join(".cache/hf/w.safetensors"), b"x");

        let models = scan(root);
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "hf/Qwen-7B");
        assert_eq!(m.format, "safetensors");
        assert_eq!(m.name, "Qwen-7B");
        assert_eq!(m.size, 6);
        assert_eq!(m.files, 2);
        assert_eq!(m.runtimes, vec!["vllm".to_string()]);
        assert_eq!(m.info, GgufInfo::default());
    }

    #[test]
    fn scan_of_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan(&tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn resolve_rejects_traversal_and_absolute_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(tmp.path().join("secret.gguf"), b"x").unwrap();

        for bad in [
            "../secret.gguf",
            "a/../../secret.gguf",
            "/etc/passwd",
            "",
            ".",
            "./",
        ] {
            assert!(resolve(&root, bad).is_err(), "{bad:?} should be rejected");
        }
        let ok_path = resolve(&root, "org/new.gguf").unwrap();
        assert!(ok_path.starts_with(root.canonicalize().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_and_delete_refuse_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        touch(&outside.join("secret.gguf"), b"x");
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        assert!(resolve(&root, "link").is_err());
        assert!(resolve(&root, "link/secret.gguf").is_err());
        assert!(delete(&root, "link/secret.gguf").is_err());
        assert!(outside.join("secret.gguf").exists());
    }

    #[test]
    fn delete_removes_the_whole_split_set_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for i in 1..=3 {
            touch(&root.join(format!("big/Big-Q4-{i:05}-of-00003.gguf")), b"x");
        }
        touch(&root.join("big/other.gguf"), b"x");
        touch(&root.join("big/Big-Q4-00001-of-00002.gguf"), b"x");

        delete(root, "big/Big-Q4-00002-of-00003.gguf").unwrap();
        for i in 1..=3 {
            assert!(!root
                .join(format!("big/Big-Q4-{i:05}-of-00003.gguf"))
                .exists());
        }
        assert!(root.join("big/other.gguf").exists());
        assert!(root.join("big/Big-Q4-00001-of-00002.gguf").exists());
    }

    #[test]
    fn delete_removes_directories_and_ignores_missing_models() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("hf/Qwen/config.json"), b"{}");
        touch(&root.join("hf/Qwen/w.safetensors"), b"x");
        touch(&root.join("hf/keep.gguf"), b"x");

        delete(root, "hf/Qwen").unwrap();
        assert!(!root.join("hf/Qwen").exists());
        assert!(root.join("hf/keep.gguf").exists());

        delete(root, "hf/Qwen").unwrap();
        delete(root, "does/not/exist.gguf").unwrap();
    }

    #[test]
    fn delete_never_removes_the_models_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(&root.join("a.gguf"), b"x");
        assert!(delete(root, ".").is_err());
        assert!(delete(root, "").is_err());
        assert!(root.join("a.gguf").exists());
    }
}
