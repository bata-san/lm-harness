//! Opaque encrypted vault (`/api/vault*`).
//!
//! The browser encrypts everything before it is sent. This module only stores and returns the
//! opaque strings it receives: it never parses, searches, indexes or logs `head` or `blob`, and
//! it never sees keys or plaintext. The only server-side metadata is `updated_at` (set here)
//! and the byte size of `blob`.
//!
//! Layout under `HARNESS_HOME/vault` (directories 0700, files 0600):
//! * `meta.json`           `{"salt","verifier"}`, set once unless replaced explicitly
//! * `{kind}/{uuid}.json`  `{"updated_at","head","blob"}` with kind in {chat, persona}
//!
//! Ids must be canonical lower-case UUIDs and are validated before they become a path. Writes
//! are atomic (private tmp file, fsync, rename). Corrupt files are skipped in listings; only the
//! file name is logged.
//!
//! Access: session only, through the same [`Admin`] extractor and [`session_only`] gate as the
//! admin API: API keys and the local setup principal get 403, anonymous requests get 401.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as UrlPath};
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::{delete, get};
use axum::{Json, Router};
use chrono::{SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::admin::{session_only, Admin};
use crate::config::HARNESS_HOME;
use crate::error::{ok, ApiError, ApiResult};

/// Max bytes for each meta field (`salt`, `verifier`).
pub const META_FIELD_MAX: usize = 1024;
/// Max bytes for an item `head`.
pub const HEAD_MAX: usize = 4096;
/// Max bytes for an item `blob` (8 MiB).
pub const BLOB_MAX: usize = 8 * 1024 * 1024;
/// Request body cap for item PUT: blob + head + 64 KiB of JSON framing.
const ITEM_BODY_MAX: usize = BLOB_MAX + HEAD_MAX + 64 * 1024;
/// Request body cap for meta PUT.
const META_BODY_MAX: usize = 16 * 1024;
/// Stored item files larger than this are treated as corrupt.
const ITEM_FILE_MAX: u64 = ITEM_BODY_MAX as u64;
const META_FILE_MAX: u64 = 16 * 1024;
/// Upper bound on directory entries inspected per listing.
const MAX_ENTRIES_PER_KIND: usize = 10_000;

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Canonical (lower-case, hyphenated) UUID only.
pub fn valid_id(id: &str) -> bool {
    id.len() == 36 && uuid::Uuid::parse_str(id).is_ok_and(|u| u.hyphenated().to_string() == id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Chat,
    Persona,
}

impl Kind {
    fn parse(s: &str) -> Result<Self, ApiError> {
        match s {
            "chat" => Ok(Kind::Chat),
            "persona" => Ok(Kind::Persona),
            _ => Err(ApiError::bad_request("不明な種類です (chat / persona のみ)")),
        }
    }

    /// The only place a kind becomes a path component.
    fn dir_name(self) -> &'static str {
        match self {
            Kind::Chat => "chat",
            Kind::Persona => "persona",
        }
    }
}

// ----------------------------------------------------------------------------- on-disk model
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredItem {
    updated_at: String,
    head: String,
    blob: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredMeta {
    salt: String,
    verifier: String,
}

#[derive(Serialize)]
pub struct ListEntry {
    id: String,
    updated_at: String,
    size: usize,
    head: String,
}

#[derive(Serialize)]
pub struct ItemView {
    id: String,
    updated_at: String,
    head: String,
    blob: String,
}

fn io_err(e: std::io::Error) -> ApiError {
    ApiError::internal(format!("保管庫の I/O に失敗しました: {e}"))
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Reads at most `cap` bytes. Anything longer is an `InvalidData` error.
fn read_capped(path: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(buf)
}

/// Reads one item for listing. Corrupt or unreadable files are logged by name and skipped.
fn read_item_lenient(path: &Path) -> Option<StoredItem> {
    let bytes = match read_capped(path, ITEM_FILE_MAX) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("vault item unreadable, skipped: {} ({})", file_name(path), e.kind());
            return None;
        }
    };
    match serde_json::from_slice::<StoredItem>(&bytes) {
        Ok(it) if it.head.len() <= HEAD_MAX && it.blob.len() <= BLOB_MAX => Some(it),
        Ok(_) => {
            tracing::warn!("vault item out of bounds, skipped: {}", file_name(path));
            None
        }
        Err(_) => {
            tracing::warn!("corrupt vault item skipped: {}", file_name(path));
            None
        }
    }
}

fn ensure_private_dir(dir: &Path) -> Result<(), ApiError> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(io_err)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io_err)
}

fn write_tmp_then_rename(tmp: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(tmp, path)?;
    // Best effort: make the rename itself durable.
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Writes `bytes` to `path` atomically: a private tmp file is fsynced and then renamed over the
/// target, so readers see either the old or the new content, never a partial file.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    let dir = path.parent().ok_or_else(|| ApiError::internal("保管庫のパスが不正です"))?;
    // The tmp name starts with '.' and does not end in ".json", so listings never see it.
    let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    if let Err(e) = write_tmp_then_rename(&tmp, path, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(e));
    }
    Ok(())
}

fn check_len(field: &str, s: &str, max: usize) -> Result<(), ApiError> {
    if s.len() > max {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, format!("{field} は {max} バイト以下にしてください")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------- store
/// File-backed vault rooted at `HARNESS_HOME/vault` (or a temp dir in tests).
pub struct Store {
    root: PathBuf,
    lock: Mutex<()>,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root, lock: Mutex::new(()) }
    }

    fn meta_path(&self) -> PathBuf {
        self.root.join("meta.json")
    }

    fn item_path(&self, kind: Kind, id: &str) -> Result<PathBuf, ApiError> {
        if !valid_id(id) {
            return Err(ApiError::bad_request("不正な ID です"));
        }
        Ok(self.root.join(kind.dir_name()).join(format!("{id}.json")))
    }

    /// Strict meta read: a corrupt file is an error, so set-once can never silently clobber it.
    fn read_meta(&self) -> Result<Option<StoredMeta>, ApiError> {
        match read_capped(&self.meta_path(), META_FILE_MAX) {
            Ok(bytes) => serde_json::from_slice::<StoredMeta>(&bytes)
                .map(Some)
                .map_err(|_| ApiError::internal("保管庫のメタ情報が破損しています")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(e)),
        }
    }

    pub fn meta(&self) -> Result<Value, ApiError> {
        let m = self.read_meta()?;
        Ok(json!({
            "v": 1,
            "salt": m.as_ref().map(|m| m.salt.clone()),
            "verifier": m.as_ref().map(|m| m.verifier.clone()),
        }))
    }

    /// Set-once unless `replace` is true. Refusing to overwrite returns 409.
    pub fn put_meta(&self, salt: String, verifier: String, replace: bool) -> Result<(), ApiError> {
        for (field, v) in [("salt", &salt), ("verifier", &verifier)] {
            if v.is_empty() {
                return Err(ApiError::bad_request(format!("{field} は空にできません")));
            }
            check_len(field, v, META_FIELD_MAX)?;
        }
        let _g = self.lock.lock();
        if !replace && self.read_meta()?.is_some() {
            return Err(ApiError(StatusCode::CONFLICT, "保管庫は既に初期化されています".into()));
        }
        ensure_private_dir(&self.root)?;
        let bytes = serde_json::to_vec(&StoredMeta { salt, verifier }).map_err(|e| ApiError::internal(e.to_string()))?;
        write_private_atomic(&self.meta_path(), &bytes)
    }

    /// Listing: id, updated_at, size (blob bytes) and head. Never includes the blob.
    /// Newest `updated_at` first.
    pub fn list(&self, kind: Kind) -> Result<Vec<ListEntry>, ApiError> {
        let dir = self.root.join(kind.dir_name());
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(io_err(e)),
        };
        let mut out: Vec<ListEntry> = vec![];
        for e in rd.flatten().take(MAX_ENTRIES_PER_KIND) {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else { continue };
            if !valid_id(id) {
                continue;
            }
            if let Some(it) = read_item_lenient(&e.path()) {
                out.push(ListEntry { id: id.to_string(), updated_at: it.updated_at, size: it.blob.len(), head: it.head });
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
        Ok(out)
    }

    pub fn get(&self, kind: Kind, id: &str) -> Result<ItemView, ApiError> {
        let path = self.item_path(kind, id)?;
        let bytes = match read_capped(&path, ITEM_FILE_MAX) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ApiError::not_found("見つかりません")),
            Err(e) => return Err(io_err(e)),
        };
        let it: StoredItem = serde_json::from_slice(&bytes).map_err(|_| {
            tracing::warn!("corrupt vault item requested: {}", file_name(&path));
            ApiError::internal("保管庫のデータが破損しています")
        })?;
        Ok(ItemView { id: id.to_string(), updated_at: it.updated_at, head: it.head, blob: it.blob })
    }

    /// Upsert. Returns the server-set `updated_at`.
    pub fn put(&self, kind: Kind, id: &str, head: String, blob: String) -> Result<String, ApiError> {
        let path = self.item_path(kind, id)?;
        check_len("head", &head, HEAD_MAX)?;
        check_len("blob", &blob, BLOB_MAX)?;
        let _g = self.lock.lock();
        let updated_at = now();
        let bytes = serde_json::to_vec(&StoredItem { updated_at: updated_at.clone(), head, blob })
            .map_err(|e| ApiError::internal(e.to_string()))?;
        ensure_private_dir(&self.root)?;
        ensure_private_dir(&self.root.join(kind.dir_name()))?;
        write_private_atomic(&path, &bytes)?;
        Ok(updated_at)
    }

    pub fn delete(&self, kind: Kind, id: &str) -> Result<(), ApiError> {
        let path = self.item_path(kind, id)?;
        let _g = self.lock.lock();
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ApiError::not_found("見つかりません")),
            Err(e) => Err(io_err(e)),
        }
    }

    /// Removes every item and the meta file.
    pub fn wipe(&self) -> Result<(), ApiError> {
        let _g = self.lock.lock();
        for kind in [Kind::Chat, Kind::Persona] {
            match std::fs::remove_dir_all(self.root.join(kind.dir_name())) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(e)),
            }
        }
        match std::fs::remove_file(self.meta_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        }
    }
}

pub static STORE: LazyLock<Store> = LazyLock::new(|| Store::new(HARNESS_HOME.join("vault")));

// ---------------------------------------------------------------------------------- handlers
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::internal(e.to_string()))
}

#[derive(Deserialize)]
struct MetaBody {
    salt: String,
    verifier: String,
    #[serde(default)]
    replace: bool,
}

#[derive(Deserialize)]
struct ItemBody {
    head: String,
    blob: String,
}

async fn get_meta(_a: Admin) -> ApiResult<Value> {
    Ok(Json(blocking(|| STORE.meta()).await??))
}

/// Body is parsed by hand so that malformed JSON also gets the `{"detail": ...}` error shape.
async fn put_meta(_a: Admin, body: Bytes) -> ApiResult<Value> {
    let req: MetaBody = serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("JSON が不正です (salt, verifier が必要です)"))?;
    blocking(move || STORE.put_meta(req.salt, req.verifier, req.replace)).await??;
    ok()
}

async fn list_items(_a: Admin, UrlPath(kind): UrlPath<String>) -> ApiResult<Vec<ListEntry>> {
    let kind = Kind::parse(&kind)?;
    Ok(Json(blocking(move || STORE.list(kind)).await??))
}

async fn get_item(_a: Admin, UrlPath((kind, id)): UrlPath<(String, String)>) -> ApiResult<ItemView> {
    let kind = Kind::parse(&kind)?;
    Ok(Json(blocking(move || STORE.get(kind, &id)).await??))
}

async fn put_item(_a: Admin, UrlPath((kind, id)): UrlPath<(String, String)>, body: Bytes) -> ApiResult<Value> {
    let kind = Kind::parse(&kind)?;
    let item: ItemBody = serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("JSON が不正です (head, blob が必要です)"))?;
    let updated_at = blocking(move || STORE.put(kind, &id, item.head, item.blob)).await??;
    Ok(Json(json!({"ok": true, "updated_at": updated_at})))
}

async fn delete_item(_a: Admin, UrlPath((kind, id)): UrlPath<(String, String)>) -> ApiResult<Value> {
    let kind = Kind::parse(&kind)?;
    blocking(move || STORE.delete(kind, &id)).await??;
    ok()
}

async fn wipe(_a: Admin) -> ApiResult<Value> {
    blocking(|| STORE.wipe()).await??;
    ok()
}

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/vault", delete(wipe))
        .route("/api/vault/meta", get(get_meta).put(put_meta).layer(DefaultBodyLimit::max(META_BODY_MAX)))
        .route("/api/vault/{kind}", get(list_items))
        .route(
            "/api/vault/{kind}/{id}",
            get(get_item).put(put_item).delete(delete_item).layer(DefaultBodyLimit::max(ITEM_BODY_MAX)),
        )
        .route_layer(middleware::from_fn(session_only))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID_A: &str = "550e8400-e29b-41d4-a716-446655440000";
    const ID_B: &str = "6ba7b810-9dad-41d1-80b4-00c04fd430c8";

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().join("vault"));
        (d, s)
    }

    #[test]
    fn router_builds() {
        let _ = router::<()>();
    }

    #[test]
    fn id_validation_blocks_traversal() {
        let (d, s) = store();
        assert!(valid_id(ID_A));
        let bad = [
            "",
            "..",
            ".",
            "../etc/passwd",
            "..\\..\\x",
            "a/b",
            "%2e%2e",
            "%2e%2e%2f%2e%2e%2fetc%2fpasswd",
            "/etc/passwd",
            "a\0b",
            "0000",
            "{550e8400-e29b-41d4-a716-446655440000}",
            "550E8400-E29B-41D4-A716-446655440000",
            "550e8400e29b41d4a716446655440000",
            "550e8400-e29b-41d4-a716-446655440000/../x",
            "../../../../../../../../../../etc/passwd",
            "550e8400-e29b-41d4-a716-44665544000.",
        ];
        for b in bad {
            assert!(!valid_id(b), "{b:?}");
            assert!(s.get(Kind::Chat, b).is_err(), "{b:?}");
            assert!(s.delete(Kind::Chat, b).is_err(), "{b:?}");
            assert!(s.put(Kind::Persona, b, String::new(), String::new()).is_err(), "{b:?}");
        }
        // nothing was created anywhere, not even the vault root
        assert!(!d.path().join("vault").exists());
    }

    #[test]
    fn kind_validation() {
        for ok_kind in ["chat", "persona"] {
            assert!(Kind::parse(ok_kind).is_ok());
        }
        for bad in ["", "Chat", "CHAT", "chats", "meta", "../chat", "chat/../persona", "chat ", "persona/x"] {
            assert!(Kind::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn size_limits() {
        let (_d, s) = store();
        let ok_head = "h".repeat(HEAD_MAX);
        assert!(s.put(Kind::Chat, ID_A, ok_head, String::new()).is_ok());
        let e = s.put(Kind::Chat, ID_A, "h".repeat(HEAD_MAX + 1), String::new()).unwrap_err();
        assert_eq!(e.0, StatusCode::PAYLOAD_TOO_LARGE);
        let e = s.put(Kind::Chat, ID_A, String::new(), "b".repeat(BLOB_MAX + 1)).unwrap_err();
        assert_eq!(e.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(s.put(Kind::Chat, ID_A, String::new(), "b".repeat(BLOB_MAX)).is_ok());
        // a rejected write leaves the previous item intact
        assert_eq!(s.get(Kind::Chat, ID_A).unwrap().blob.len(), BLOB_MAX);

        let e = s.put_meta("s".repeat(META_FIELD_MAX + 1), "v".into(), false).unwrap_err();
        assert_eq!(e.0, StatusCode::PAYLOAD_TOO_LARGE);
        let e = s.put_meta("s".into(), "v".repeat(META_FIELD_MAX + 1), false).unwrap_err();
        assert_eq!(e.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(s.put_meta("s".repeat(META_FIELD_MAX), "v".into(), false).is_ok());
    }

    #[test]
    fn meta_set_once_vs_replace() {
        let (_d, s) = store();
        let m = s.meta().unwrap();
        assert_eq!(m, json!({"v": 1, "salt": null, "verifier": null}));

        assert!(s.put_meta(String::new(), "v".into(), false).is_err());
        s.put_meta("salt1".into(), "ver1".into(), false).unwrap();
        assert_eq!(s.meta().unwrap(), json!({"v": 1, "salt": "salt1", "verifier": "ver1"}));

        // set-once: a second write without replace is refused and changes nothing
        let e = s.put_meta("salt2".into(), "ver2".into(), false).unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        assert_eq!(s.meta().unwrap()["salt"], "salt1");

        // explicit replace overwrites
        s.put_meta("salt2".into(), "ver2".into(), true).unwrap();
        assert_eq!(s.meta().unwrap(), json!({"v": 1, "salt": "salt2", "verifier": "ver2"}));
    }

    #[test]
    fn put_get_list_delete_roundtrip() {
        let (_d, s) = store();
        assert!(s.list(Kind::Chat).unwrap().is_empty());
        assert_eq!(s.get(Kind::Chat, ID_A).err().expect("expected an error").0, StatusCode::NOT_FOUND);

        let t1 = s.put(Kind::Chat, ID_A, "head-1".into(), "blob-1".into()).unwrap();
        assert!(t1.ends_with('Z') && t1.len() == 24, "RFC3339 UTC with millis: {t1}");

        let got = s.get(Kind::Chat, ID_A).unwrap();
        assert_eq!((got.id.as_str(), got.head.as_str(), got.blob.as_str(), got.updated_at.as_str()), (ID_A, "head-1", "blob-1", t1.as_str()));

        // kinds are separate namespaces
        assert!(s.list(Kind::Persona).unwrap().is_empty());
        assert_eq!(s.get(Kind::Persona, ID_A).err().expect("expected an error").0, StatusCode::NOT_FOUND);

        // overwrite updates content and timestamp
        std::thread::sleep(std::time::Duration::from_millis(3));
        let t2 = s.put(Kind::Chat, ID_A, "head-2".into(), "blob-2".into()).unwrap();
        assert!(t2 > t1);
        assert_eq!(s.get(Kind::Chat, ID_A).unwrap().blob, "blob-2");

        s.delete(Kind::Chat, ID_A).unwrap();
        assert_eq!(s.get(Kind::Chat, ID_A).err().expect("expected an error").0, StatusCode::NOT_FOUND);
        assert_eq!(s.delete(Kind::Chat, ID_A).unwrap_err().0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn listing_is_newest_first_and_never_contains_blob() {
        let (_d, s) = store();
        let secret_blob = "BLOB-SECRET-PAYLOAD-7f3a";
        s.put(Kind::Chat, ID_A, "head-a".into(), secret_blob.into()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        s.put(Kind::Chat, ID_B, "head-b".into(), format!("{secret_blob}-2")).unwrap();

        let list = s.list(Kind::Chat).unwrap();
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), [ID_B, ID_A]);
        assert_eq!(list[1].size, secret_blob.len());
        assert_eq!(list[1].head, "head-a");

        let rendered = serde_json::to_string(&list).unwrap();
        assert!(!rendered.contains(secret_blob), "listing leaked a blob: {rendered}");
        let v: Value = serde_json::from_str(&rendered).unwrap();
        for e in v.as_array().unwrap() {
            let mut keys: Vec<&str> = e.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["head", "id", "size", "updated_at"]);
        }
    }

    #[test]
    fn wipe_removes_items_and_meta() {
        let (d, s) = store();
        s.put_meta("s".into(), "v".into(), false).unwrap();
        s.put(Kind::Chat, ID_A, "h".into(), "b".into()).unwrap();
        s.put(Kind::Persona, ID_B, "h".into(), "b".into()).unwrap();
        s.wipe().unwrap();
        assert!(s.meta().unwrap()["salt"].is_null());
        assert!(s.list(Kind::Chat).unwrap().is_empty());
        assert!(s.list(Kind::Persona).unwrap().is_empty());
        assert!(!d.path().join("vault/meta.json").exists());
        // wiping an empty vault is fine too
        s.wipe().unwrap();
    }

    #[test]
    fn corrupt_files_are_skipped_not_fatal() {
        let (d, s) = store();
        s.put(Kind::Chat, ID_A, "h".into(), "b".into()).unwrap();
        let dir = d.path().join("vault/chat");
        std::fs::write(dir.join(format!("{ID_B}.json")), "{not json").unwrap();
        std::fs::write(dir.join("notauuid.json"), "{}").unwrap();
        std::fs::write(dir.join("readme.txt"), "x").unwrap();
        // valid JSON but with an unexpected field: treated as corrupt
        std::fs::write(dir.join("6ba7b810-9dad-41d1-80b4-00c04fd430c9.json"), r#"{"updated_at":"x","head":"h","blob":"b","title":"plain"}"#).unwrap();
        // wrong types
        std::fs::write(dir.join("6ba7b810-9dad-41d1-80b4-00c04fd430ca.json"), r#"{"updated_at":1,"head":"h","blob":"b"}"#).unwrap();

        let list = s.list(Kind::Chat).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, ID_A);
        assert_eq!(s.get(Kind::Chat, ID_B).err().expect("expected an error").0, StatusCode::INTERNAL_SERVER_ERROR);
        // a corrupt file can be overwritten by a normal put
        s.put(Kind::Chat, ID_B, "h2".into(), "b2".into()).unwrap();
        assert_eq!(s.get(Kind::Chat, ID_B).unwrap().head, "h2");
    }

    #[test]
    fn corrupt_meta_is_not_silently_clobbered() {
        let (d, s) = store();
        std::fs::create_dir_all(d.path().join("vault")).unwrap();
        std::fs::write(d.path().join("vault/meta.json"), "garbage").unwrap();
        assert_eq!(s.meta().unwrap_err().0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(s.put_meta("s".into(), "v".into(), false).unwrap_err().0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(std::fs::read_to_string(d.path().join("vault/meta.json")).unwrap(), "garbage");
        // explicit replace is the way out
        s.put_meta("s".into(), "v".into(), true).unwrap();
    }

    #[test]
    fn disk_contains_only_the_given_strings() {
        let (d, s) = store();
        let head = "HEAD-ciphertext+/==\"quoted\"\\slash";
        let blob = "BLOB-ciphertext-base64==\u{00e9}\n";
        s.put(Kind::Persona, ID_A, head.into(), blob.into()).unwrap();
        s.put_meta("SALT-b64".into(), "VERIFIER-b64".into(), false).unwrap();

        let item_path = d.path().join("vault/persona").join(format!("{ID_A}.json"));
        let raw = std::fs::read(&item_path).unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["blob", "head", "updated_at"], "no extra fields on disk");
        assert_eq!(v["head"], head);
        assert_eq!(v["blob"], blob);
        // nothing else readable: no id, no kind, no plaintext metadata
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.contains(ID_A));
        assert!(!text.contains("persona"));

        let meta_raw: Value = serde_json::from_slice(&std::fs::read(d.path().join("vault/meta.json")).unwrap()).unwrap();
        assert_eq!(meta_raw, json!({"salt": "SALT-b64", "verifier": "VERIFIER-b64"}));

        // the directory holds exactly the one item file (no leftover tmp files)
        let names: Vec<String> = std::fs::read_dir(d.path().join("vault/persona")).unwrap().map(|e| file_name(&e.unwrap().path())).collect();
        assert_eq!(names, [format!("{ID_A}.json")]);
    }

    #[cfg(unix)]
    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let (d, s) = store();
        s.put_meta("s".into(), "v".into(), false).unwrap();
        s.put(Kind::Chat, ID_A, "h".into(), "b".into()).unwrap();
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let vault = d.path().join("vault");
        assert_eq!(mode(vault.clone()), 0o700);
        assert_eq!(mode(vault.join("chat")), 0o700);
        assert_eq!(mode(vault.join("meta.json")), 0o600);
        assert_eq!(mode(vault.join("chat").join(format!("{ID_A}.json"))), 0o600);
    }

    #[tokio::test]
    async fn vault_routes_reject_api_keys_and_anonymous() {
        use crate::auth::Principal;
        use axum::middleware::from_fn;

        async fn inject(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
            let kind = req.headers().get("x-test-kind").and_then(|v| v.to_str().ok()).map(str::to_owned);
            let p = match kind.as_deref() {
                Some("session") => Some(Principal { kind: "session", name: "admin".into(), session_id: Some("s1".into()) }),
                Some("api_key") => Some(Principal { kind: "api_key", name: "k".into(), session_id: None }),
                Some("local") => Some(Principal { kind: "local", name: "local".into(), session_id: None }),
                _ => None,
            };
            if let Some(p) = p {
                req.extensions_mut().insert(p);
            }
            next.run(req).await
        }

        // Only rejection paths are exercised here: a session call would reach the real HARNESS_HOME store.
        let app: Router = router::<()>().layer(from_fn(inject));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap() });
        let http = reqwest::Client::new();
        let url = format!("http://{addr}/api/vault/meta");
        for (kind, want) in [("api_key", 403), ("local", 403)] {
            let st = http.get(&url).header("x-test-kind", kind).send().await.unwrap().status().as_u16();
            assert_eq!(st, want, "{kind}");
            let st = http
                .put(format!("http://{addr}/api/vault/chat/{ID_A}"))
                .header("x-test-kind", kind)
                .body(r#"{"head":"h","blob":"b"}"#)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16();
            assert_eq!(st, want, "{kind} put");
        }
        let st = http.get(&url).send().await.unwrap().status().as_u16();
        assert_eq!(st, 401);
    }

    /// Oversize bodies are refused with 413 before anything is written. Only rejection paths are
    /// used, so the global store under HARNESS_HOME is never touched.
    #[tokio::test]
    async fn oversize_item_bodies_are_refused() {
        use crate::auth::Principal;
        use axum::middleware::from_fn;

        async fn as_session(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
            req.extensions_mut().insert(Principal { kind: "session", name: "admin".into(), session_id: Some("s1".into()) });
            next.run(req).await
        }

        let app: Router = router::<()>().layer(from_fn(as_session));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap() });
        let http = reqwest::Client::new();
        let url = format!("http://{addr}/api/vault/chat/{ID_A}");

        // blob one byte over the cap: rejected by the handler's size check
        let blob = "a".repeat(BLOB_MAX + 1);
        let st = http.put(&url).body(format!(r#"{{"head":"h","blob":"{blob}"}}"#)).send().await.unwrap().status().as_u16();
        assert_eq!(st, 413);

        // body beyond the transport cap: rejected by the body-limit layer
        let body = format!(r#"{{"head":"h","blob":"{}"}}"#, "a".repeat(ITEM_BODY_MAX));
        let st = http.put(&url).body(body).send().await.unwrap().status().as_u16();
        assert_eq!(st, 413);

        // head one byte over the cap
        let head = "h".repeat(HEAD_MAX + 1);
        let st = http.put(&url).body(format!(r#"{{"head":"{head}","blob":"b"}}"#)).send().await.unwrap().status().as_u16();
        assert_eq!(st, 413);

        // malformed JSON gets the {"detail"} shape
        let resp = http.put(&url).body("{not json").send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 400);
        let v: Value = resp.json().await.unwrap();
        assert!(v["detail"].is_string());
    }
}
