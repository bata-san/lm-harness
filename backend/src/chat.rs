//! Persistence for the chat UI (conversations + personas). Generation itself is done by the
//! frontend against `/v1/chat/completions`; this module only stores data.
//!
//! Layout under `HARNESS_HOME`:
//! * `chats/<uuid>.json`  one file per conversation (atomic tmp + rename writes)
//! * `personas.json`      array of personas, seeded with built-ins on first run
//!
//! Chat ids are validated to be canonical lower-case hyphenated UUIDs before they are ever
//! turned into a path, so client input can never escape the `chats/` directory. Corrupt files
//! are skipped (and logged); they never break listing or search.
//!
//! Routes: /api/chats*, /api/personas*

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use chrono::{SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::{read_json_file, write_json_file, HARNESS_HOME};
use crate::error::{ok, ApiError, ApiResult};
use crate::state::AppState;

const MAX_MESSAGES: usize = 5000;
const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_CHAT_FILE: u64 = 64 * 1024 * 1024;
const MAX_LIST: usize = 5000;
const MAX_SEARCH_FILES: usize = 2000;
const MAX_SEARCH_RESULTS: usize = 50;
const MAX_PERSONAS: usize = 200;
const MAX_TITLE: usize = 200;
const MAX_SYSTEM: usize = 100_000;

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Canonical (lower-case, hyphenated) UUID only.
pub fn valid_id(id: &str) -> bool {
    id.len() == 36 && uuid::Uuid::parse_str(id).is_ok_and(|u| u.hyphenated().to_string() == id)
}

// ------------------------------------------------------------------------------ data model
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Params {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl Params {
    fn validate(&self) -> Result<(), ApiError> {
        if self.temperature.is_some_and(|t| !(0.0..=5.0).contains(&t)) {
            return Err(ApiError::bad_request("temperature は 0〜5 で指定してください"));
        }
        if self.top_p.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
            return Err(ApiError::bad_request("top_p は 0〜1 で指定してください"));
        }
        if self.max_tokens.is_some_and(|t| t == 0 || t > 10_000_000) {
            return Err(ApiError::bad_request("max_tokens が範囲外です"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    #[serde(default)]
    pub id: String,
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tps: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub persona_id: Option<String>,
    pub model: Option<String>,
    pub system: String,
    pub params: Params,
    pub pinned: bool,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<Message>,
}

#[derive(Serialize)]
struct ChatSummary {
    id: String,
    title: String,
    persona_id: Option<String>,
    model: Option<String>,
    created_at: String,
    updated_at: String,
    message_count: usize,
    pinned: bool,
}

impl From<&Chat> for ChatSummary {
    fn from(c: &Chat) -> Self {
        Self {
            id: c.id.clone(),
            title: c.title.clone(),
            persona_id: c.persona_id.clone(),
            model: c.model.clone(),
            created_at: c.created_at.clone(),
            updated_at: c.updated_at.clone(),
            message_count: c.messages.len(),
            pinned: c.pinned,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Persona {
    pub id: String,
    pub name: String,
    pub emoji: String,
    pub description: String,
    pub system: String,
    pub model: Option<String>,
    pub params: Params,
    pub created_at: String,
    pub builtin: bool,
}

fn builtin_personas() -> Vec<Persona> {
    let ts = now();
    let mk = |id: &str, name: &str, emoji: &str, desc: &str, system: &str, temperature: f64| Persona {
        id: id.into(),
        name: name.into(),
        emoji: emoji.into(),
        description: desc.into(),
        system: system.into(),
        model: None,
        params: Params { temperature: Some(temperature), ..Default::default() },
        created_at: ts.clone(),
        builtin: true,
    };
    vec![
        mk(
            "builtin-general",
            "汎用アシスタント",
            "💬",
            "何でも相談できる標準アシスタント",
            "あなたは親切で正確なアシスタントです。ユーザーの言語で、簡潔かつ丁寧に回答してください。不確かなことは推測であると明示してください。",
            0.7,
        ),
        mk(
            "builtin-programmer",
            "プログラマ",
            "👨‍💻",
            "コード生成・レビュー・デバッグ",
            "あなたは経験豊富なソフトウェアエンジニアです。動作するコードを Markdown のコードブロックで示し、要点を短く説明してください。エッジケースとセキュリティにも触れてください。",
            0.2,
        ),
        mk(
            "builtin-translator",
            "翻訳 (日英)",
            "🌐",
            "日本語と英語の相互翻訳",
            "あなたは日英翻訳者です。入力が日本語なら自然な英語に、英語なら自然な日本語に翻訳してください。翻訳文のみを出力し、説明は求められた場合だけ付けてください。",
            0.3,
        ),
        mk(
            "builtin-summarizer",
            "要約",
            "📝",
            "長文を要点にまとめる",
            "あなたは要約の専門家です。入力された文章の要点を、元の言語で、箇条書き 3〜7 項目に簡潔にまとめてください。事実を追加せず、数値や固有名詞は正確に保ってください。",
            0.3,
        ),
    ]
}

// ---------------------------------------------------------------------------------- store
/// File-backed store rooted at a directory (`HARNESS_HOME` in production).
pub struct Store {
    root: PathBuf,
    lock: Mutex<()>,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root, lock: Mutex::new(()) }
    }

    fn chats_dir(&self) -> PathBuf {
        self.root.join("chats")
    }

    fn chat_path(&self, id: &str) -> Result<PathBuf, ApiError> {
        if !valid_id(id) {
            return Err(ApiError::bad_request("不正な ID です"));
        }
        Ok(self.chats_dir().join(format!("{id}.json")))
    }

    fn read_chat_file(path: &Path) -> Option<Chat> {
        match std::fs::metadata(path) {
            Ok(m) if m.len() > MAX_CHAT_FILE => {
                tracing::warn!("chat file too large, skipped: {}", path.display());
                return None;
            }
            Ok(_) => {}
            Err(_) => return None,
        }
        let bytes = std::fs::read(path).ok()?;
        match serde_json::from_slice::<Chat>(&bytes) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("corrupt chat file skipped: {} ({e})", path.display());
                None
            }
        }
    }

    pub fn load(&self, id: &str) -> Result<Chat, ApiError> {
        let path = self.chat_path(id)?;
        if !path.is_file() {
            return Err(ApiError::not_found("チャットが見つかりません"));
        }
        let mut chat = Self::read_chat_file(&path).ok_or_else(|| ApiError::internal("チャットファイルが破損しています"))?;
        chat.id = id.to_string(); // the file name is authoritative
        Ok(chat)
    }

    fn save(&self, chat: &Chat) -> Result<(), ApiError> {
        let path = self.chat_path(&chat.id)?;
        std::fs::create_dir_all(self.chats_dir()).map_err(|e| ApiError::internal(e.to_string()))?;
        write_json_file(&path, chat)?;
        Ok(())
    }

    /// All readable chats (corrupt files skipped), newest first.
    fn all(&self) -> Vec<Chat> {
        let mut out = vec![];
        let Ok(rd) = std::fs::read_dir(self.chats_dir()) else { return out };
        for e in rd.flatten().take(MAX_LIST * 2) {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else { continue };
            if !valid_id(id) {
                continue;
            }
            if let Some(mut c) = Self::read_chat_file(&e.path()) {
                c.id = id.to_string();
                out.push(c);
                if out.len() >= MAX_LIST {
                    break;
                }
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| b.id.cmp(&a.id)));
        out
    }

    pub fn list(&self) -> Vec<Value> {
        self.all().iter().map(|c| json!(ChatSummary::from(c))).collect()
    }

    pub fn create(&self, body: &Value) -> Result<Chat, ApiError> {
        let _g = self.lock.lock();
        let ts = now();
        let mut chat = Chat { id: new_id(), created_at: ts.clone(), updated_at: ts, title: "新しいチャット".into(), ..Default::default() };
        apply_meta(&mut chat, body)?;
        self.save(&chat)?;
        Ok(chat)
    }

    pub fn update_meta(&self, id: &str, patch: &Value) -> Result<Chat, ApiError> {
        let _g = self.lock.lock();
        let mut chat = self.load(id)?;
        apply_meta(&mut chat, patch)?;
        chat.updated_at = now();
        self.save(&chat)?;
        Ok(chat)
    }

    pub fn replace_messages(&self, id: &str, mut messages: Vec<Message>) -> Result<Chat, ApiError> {
        if messages.len() > MAX_MESSAGES {
            return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, format!("メッセージは最大 {MAX_MESSAGES} 件です")));
        }
        let ts = now();
        let mut seen = std::collections::HashSet::new();
        for m in &mut messages {
            if m.id.is_empty() || m.id.len() > 64 || !seen.insert(m.id.clone()) {
                m.id = new_id();
                seen.insert(m.id.clone());
            }
            if m.created_at.is_empty() {
                m.created_at = ts.clone();
            }
        }
        let _g = self.lock.lock();
        let mut chat = self.load(id)?;
        chat.messages = messages;
        chat.updated_at = ts;
        self.save(&chat)?;
        Ok(chat)
    }

    pub fn delete(&self, id: &str) -> Result<(), ApiError> {
        let _g = self.lock.lock();
        let path = self.chat_path(id)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ApiError::not_found("チャットが見つかりません")),
            Err(e) => Err(ApiError::internal(e.to_string())),
        }
    }

    pub fn search(&self, q: &str) -> Vec<Value> {
        let needle = q.to_lowercase();
        if needle.is_empty() {
            return vec![];
        }
        let mut out = vec![];
        for c in self.all().into_iter().take(MAX_SEARCH_FILES) {
            let mut hit: Option<(&str, String)> = None;
            if c.title.to_lowercase().contains(&needle) {
                hit = Some(("title", c.title.clone()));
            } else {
                for m in &c.messages {
                    if let Some(snip) = snippet(&m.content, &needle) {
                        hit = Some(("content", snip));
                        break;
                    }
                }
            }
            if let Some((kind, snip)) = hit {
                out.push(json!({"id": c.id, "title": c.title, "updated_at": c.updated_at, "match": kind, "snippet": snip}));
                if out.len() >= MAX_SEARCH_RESULTS {
                    break;
                }
            }
        }
        out
    }

    // ---------------------------------------------------------------------- personas
    fn personas_path(&self) -> PathBuf {
        self.root.join("personas.json")
    }

    fn read_personas(&self) -> Vec<Persona> {
        let path = self.personas_path();
        if !path.exists() {
            let seed = builtin_personas();
            if let Err(e) = std::fs::create_dir_all(&self.root).map_err(anyhow::Error::from).and_then(|_| write_json_file(&path, &seed)) {
                tracing::warn!("cannot seed personas.json: {e:#}");
            }
            return seed;
        }
        match read_json_file::<Vec<Persona>>(&path) {
            Some(v) => v,
            None => {
                tracing::warn!("corrupt personas.json: using built-in defaults (file left untouched until next save)");
                builtin_personas()
            }
        }
    }

    fn write_personas(&self, list: &[Persona]) -> Result<(), ApiError> {
        std::fs::create_dir_all(&self.root).map_err(|e| ApiError::internal(e.to_string()))?;
        write_json_file(&self.personas_path(), list)?;
        Ok(())
    }

    pub fn personas(&self) -> Vec<Persona> {
        let _g = self.lock.lock();
        self.read_personas()
    }

    pub fn create_persona(&self, body: &Value) -> Result<Persona, ApiError> {
        let _g = self.lock.lock();
        let mut list = self.read_personas();
        if list.len() >= MAX_PERSONAS {
            return Err(ApiError::bad_request("ペルソナが多すぎます"));
        }
        let mut p = Persona { id: new_id(), created_at: now(), ..Default::default() };
        apply_persona(&mut p, body, true)?;
        list.push(p.clone());
        self.write_personas(&list)?;
        Ok(p)
    }

    /// Updating a built-in persona leaves it untouched and stores an editable copy instead.
    pub fn update_persona(&self, id: &str, body: &Value) -> Result<Persona, ApiError> {
        let _g = self.lock.lock();
        let mut list = self.read_personas();
        let idx = persona_index(&list, id)?;
        if list[idx].builtin {
            let mut copy = list[idx].clone();
            copy.id = new_id();
            copy.builtin = false;
            copy.created_at = now();
            apply_persona(&mut copy, body, false)?;
            list.push(copy.clone());
            self.write_personas(&list)?;
            return Ok(copy);
        }
        apply_persona(&mut list[idx], body, false)?;
        let p = list[idx].clone();
        self.write_personas(&list)?;
        Ok(p)
    }

    pub fn delete_persona(&self, id: &str) -> Result<(), ApiError> {
        let _g = self.lock.lock();
        let mut list = self.read_personas();
        let idx = persona_index(&list, id)?;
        if list[idx].builtin {
            return Err(ApiError(StatusCode::FORBIDDEN, "組み込みペルソナは削除できません".into()));
        }
        list.remove(idx);
        self.write_personas(&list)
    }
}

fn persona_index(list: &[Persona], id: &str) -> Result<usize, ApiError> {
    if id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(ApiError::bad_request("不正な ID です"));
    }
    list.iter().position(|p| p.id == id).ok_or_else(|| ApiError::not_found("ペルソナが見つかりません"))
}

fn snippet(content: &str, needle_lower: &str) -> Option<String> {
    let lower = content.to_lowercase();
    let pos = lower.find(needle_lower)?;
    // `lower` can differ in byte length from `content` for exotic characters; map via chars.
    let char_pos = lower[..pos].chars().count();
    let chars: Vec<char> = content.chars().collect();
    let start = char_pos.saturating_sub(40);
    let end = (char_pos + needle_lower.chars().count() + 80).min(chars.len());
    Some(chars[start.min(end)..end].iter().collect::<String>().replace('\n', " "))
}

fn opt_string(v: &Value, key: &str, max: usize) -> Result<Option<Option<String>>, ApiError> {
    match v.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) if s.chars().count() <= max => Ok(Some(Some(s.clone()))),
        Some(_) => Err(ApiError::bad_request(format!("{key} が不正です"))),
    }
}

fn merge_params(target: &mut Params, v: &Value) -> Result<(), ApiError> {
    match v {
        Value::Null => *target = Params::default(),
        Value::Object(m) => {
            let mut cur = serde_json::to_value(&*target).map_err(|e| ApiError::internal(e.to_string()))?;
            let Value::Object(cm) = &mut cur else { unreachable!() };
            for (k, val) in m {
                if !matches!(k.as_str(), "temperature" | "top_p" | "max_tokens") {
                    continue;
                }
                if val.is_null() {
                    cm.remove(k);
                } else {
                    cm.insert(k.clone(), val.clone());
                }
            }
            *target = serde_json::from_value(cur).map_err(|_| ApiError::bad_request("params が不正です"))?;
        }
        _ => return Err(ApiError::bad_request("params はオブジェクトで指定してください")),
    }
    target.validate()
}

/// Shared by create and partial update. Only present keys are touched.
fn apply_meta(chat: &mut Chat, v: &Value) -> Result<(), ApiError> {
    if !v.is_object() {
        return Err(ApiError::bad_request("JSON オブジェクトを指定してください"));
    }
    if let Some(Some(t)) = opt_string(v, "title", MAX_TITLE)? {
        chat.title = t;
    }
    if let Some(p) = opt_string(v, "persona_id", 64)? {
        chat.persona_id = p.filter(|s| !s.is_empty());
    }
    if let Some(m) = opt_string(v, "model", 300)? {
        chat.model = m.filter(|s| !s.is_empty());
    }
    if let Some(s) = opt_string(v, "system", MAX_SYSTEM)? {
        chat.system = s.unwrap_or_default();
    }
    if let Some(p) = v.get("params") {
        merge_params(&mut chat.params, p)?;
    }
    match v.get("pinned") {
        None => {}
        Some(Value::Bool(b)) => chat.pinned = *b,
        Some(_) => return Err(ApiError::bad_request("pinned は真偽値です")),
    }
    Ok(())
}

fn apply_persona(p: &mut Persona, v: &Value, creating: bool) -> Result<(), ApiError> {
    if !v.is_object() {
        return Err(ApiError::bad_request("JSON オブジェクトを指定してください"));
    }
    if let Some(Some(n)) = opt_string(v, "name", 100)? {
        p.name = n;
    }
    if creating && p.name.trim().is_empty() {
        return Err(ApiError::bad_request("name は必須です"));
    }
    if p.name.trim().is_empty() && v.get("name").is_some() {
        return Err(ApiError::bad_request("name は空にできません"));
    }
    if let Some(Some(e)) = opt_string(v, "emoji", 16)? {
        p.emoji = e;
    }
    if let Some(Some(d)) = opt_string(v, "description", 1000)? {
        p.description = d;
    }
    if let Some(s) = opt_string(v, "system", MAX_SYSTEM)? {
        p.system = s.unwrap_or_default();
    }
    if let Some(m) = opt_string(v, "model", 300)? {
        p.model = m.filter(|s| !s.is_empty());
    }
    if let Some(pp) = v.get("params") {
        merge_params(&mut p.params, pp)?;
    }
    Ok(())
}

pub static STORE: LazyLock<Store> = LazyLock::new(|| Store::new(HARNESS_HOME.clone()));

// ----------------------------------------------------------------------------------- export
fn export_markdown(c: &Chat) -> String {
    let mut s = format!("# {}\n\n", c.title);
    s.push_str(&format!("- id: {}\n- created: {}\n- updated: {}\n", c.id, c.created_at, c.updated_at));
    if let Some(m) = &c.model {
        s.push_str(&format!("- model: {m}\n"));
    }
    s.push('\n');
    if !c.system.is_empty() {
        s.push_str(&format!("## System\n\n{}\n\n", c.system));
    }
    for m in &c.messages {
        let who = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
        };
        s.push_str(&format!("## {who}\n\n{}\n\n", m.content));
    }
    s
}

// ---------------------------------------------------------------------------------- handlers
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::internal(e.to_string()))
}

async fn list_chats() -> ApiResult<Vec<Value>> {
    Ok(Json(blocking(|| STORE.list()).await?))
}

async fn create_chat(body: Option<Json<Value>>) -> ApiResult<Chat> {
    let body = body.map(|b| b.0).unwrap_or_else(|| Value::Object(Map::new()));
    Ok(Json(blocking(move || STORE.create(&body)).await??))
}

async fn get_chat(UrlPath(id): UrlPath<String>) -> ApiResult<Chat> {
    Ok(Json(blocking(move || STORE.load(&id)).await??))
}

async fn put_chat(UrlPath(id): UrlPath<String>, Json(patch): Json<Value>) -> ApiResult<Chat> {
    Ok(Json(blocking(move || STORE.update_meta(&id, &patch)).await??))
}

async fn put_messages(UrlPath(id): UrlPath<String>, Json(messages): Json<Vec<Message>>) -> ApiResult<Value> {
    let chat = blocking(move || STORE.replace_messages(&id, messages)).await??;
    Ok(Json(json!({"ok": true, "updated_at": chat.updated_at, "message_count": chat.messages.len()})))
}

async fn delete_chat(UrlPath(id): UrlPath<String>) -> ApiResult<Value> {
    blocking(move || STORE.delete(&id)).await??;
    ok()
}

#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

async fn export_chat(UrlPath(id): UrlPath<String>, Query(q): Query<ExportQuery>) -> Result<Response, ApiError> {
    let chat = blocking(move || STORE.load(&id)).await??;
    let (body, ctype, ext) = match q.format.as_deref().unwrap_or("json") {
        "json" => (serde_json::to_string_pretty(&chat).map_err(|e| ApiError::internal(e.to_string()))?, "application/json; charset=utf-8", "json"),
        "md" | "markdown" => (export_markdown(&chat), "text/markdown; charset=utf-8", "md"),
        _ => return Err(ApiError::bad_request("format は json か md です")),
    };
    let disp = format!("attachment; filename=\"chat-{}.{ext}\"", chat.id);
    Ok(([(header::CONTENT_TYPE, ctype.to_string()), (header::CONTENT_DISPOSITION, disp)], body).into_response())
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

async fn search_chats(Query(q): Query<SearchQuery>) -> ApiResult<Vec<Value>> {
    let needle: String = q.q.unwrap_or_default().trim().chars().take(200).collect();
    Ok(Json(blocking(move || STORE.search(&needle)).await?))
}

async fn list_personas() -> ApiResult<Vec<Persona>> {
    Ok(Json(blocking(|| STORE.personas()).await?))
}

async fn create_persona(Json(body): Json<Value>) -> ApiResult<Persona> {
    Ok(Json(blocking(move || STORE.create_persona(&body)).await??))
}

async fn update_persona(UrlPath(id): UrlPath<String>, Json(body): Json<Value>) -> ApiResult<Persona> {
    Ok(Json(blocking(move || STORE.update_persona(&id, &body)).await??))
}

async fn delete_persona(UrlPath(id): UrlPath<String>) -> ApiResult<Value> {
    blocking(move || STORE.delete_persona(&id)).await??;
    ok()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/chats", get(list_chats).post(create_chat))
        .route("/api/chats/search", get(search_chats))
        .route("/api/chats/{id}", get(get_chat).put(put_chat).delete(delete_chat))
        .route("/api/chats/{id}/messages", put(put_messages).layer(DefaultBodyLimit::max(MAX_BODY)))
        .route("/api/chats/{id}/export", get(export_chat))
        .route("/api/personas", get(list_personas).post(create_persona))
        .route("/api/personas/{id}", put(update_persona).delete(delete_persona))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_builds_without_route_conflicts() {
        let _ = router();
    }

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().to_path_buf());
        (d, s)
    }

    fn msg(role: Role, content: &str) -> Message {
        Message { id: String::new(), role, content: content.into(), created_at: String::new(), model: None, usage: None, tps: None }
    }

    #[test]
    fn id_validation_blocks_traversal() {
        let (_d, s) = store();
        assert!(valid_id(&new_id()));
        for bad in ["", "..", "../etc/passwd", "a/b", "%2e%2e", "0000", "{550e8400-e29b-41d4-a716-446655440000}", "550E8400-E29B-41D4-A716-446655440000", "550e8400e29b41d4a716446655440000", "550e8400-e29b-41d4-a716-446655440000/../x"] {
            assert!(!valid_id(bad), "{bad}");
            assert!(s.load(bad).is_err());
            assert!(s.delete(bad).is_err());
            assert!(s.update_meta(bad, &json!({})).is_err());
            assert!(s.replace_messages(bad, vec![]).is_err());
        }
    }

    #[test]
    fn roundtrip_and_meta_update() {
        let (_d, s) = store();
        let c = s.create(&json!({"title": "t", "model": "m", "system": "sys", "persona_id": "builtin-general"})).unwrap();
        assert!(valid_id(&c.id));
        let c2 = s.replace_messages(&c.id, vec![msg(Role::User, "hello"), msg(Role::Assistant, "world")]).unwrap();
        assert_eq!(c2.messages.len(), 2);
        assert!(!c2.messages[0].id.is_empty() && c2.messages[0].id != c2.messages[1].id);
        let loaded = s.load(&c.id).unwrap();
        assert_eq!(loaded.messages[1].content, "world");
        assert_eq!(loaded.system, "sys");
        let u = s.update_meta(&c.id, &json!({"title": "new", "pinned": true, "params": {"temperature": 0.5}, "model": null})).unwrap();
        assert_eq!((u.title.as_str(), u.pinned, u.model.clone(), u.params.temperature), ("new", true, None, Some(0.5)));
        assert_eq!(u.messages.len(), 2, "metadata update keeps messages");
        assert!(s.update_meta(&c.id, &json!({"params": {"temperature": 99}})).is_err());
        assert!(s.update_meta(&c.id, &json!({"pinned": "yes"})).is_err());
        s.delete(&c.id).unwrap();
        assert!(s.load(&c.id).is_err());
        assert!(s.delete(&c.id).is_err());
    }

    #[test]
    fn message_cap() {
        let (_d, s) = store();
        let c = s.create(&json!({})).unwrap();
        let many: Vec<Message> = (0..MAX_MESSAGES + 1).map(|_| msg(Role::User, "x")).collect();
        assert_eq!(s.replace_messages(&c.id, many).unwrap_err().0, StatusCode::PAYLOAD_TOO_LARGE);
        let ok: Vec<Message> = (0..MAX_MESSAGES).map(|_| msg(Role::User, "x")).collect();
        assert!(s.replace_messages(&c.id, ok).is_ok());
    }

    #[test]
    fn corrupt_files_do_not_break_listing_or_search() {
        let (d, s) = store();
        let good = s.create(&json!({"title": "needle chat"})).unwrap();
        let bad_id = new_id();
        std::fs::write(d.path().join("chats").join(format!("{bad_id}.json")), "{not json").unwrap();
        std::fs::write(d.path().join("chats/notauuid.json"), "{}").unwrap();
        std::fs::write(d.path().join("chats/readme.txt"), "x").unwrap();
        let list = s.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], good.id.as_str());
        assert_eq!(s.search("NEEDLE").len(), 1);
        assert!(s.load(&bad_id).is_err());
    }

    #[test]
    fn list_newest_first_with_counts() {
        let (_d, s) = store();
        let a = s.create(&json!({"title": "a"})).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = s.create(&json!({"title": "b"})).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.replace_messages(&a.id, vec![msg(Role::User, "x")]).unwrap();
        let l = s.list();
        assert_eq!(l[0]["id"], a.id.as_str());
        assert_eq!(l[0]["message_count"], 1);
        assert_eq!(l[1]["id"], b.id.as_str());
        assert!(l[0].get("messages").is_none());
    }

    #[test]
    fn search_content_and_snippet() {
        let (_d, s) = store();
        let c = s.create(&json!({"title": "x"})).unwrap();
        s.replace_messages(&c.id, vec![msg(Role::User, "日本語のテキストに Rust が含まれる")]).unwrap();
        let r = s.search("rust");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["match"], "content");
        assert!(r[0]["snippet"].as_str().unwrap().contains("Rust"));
        assert!(s.search("zzz").is_empty());
        assert!(s.search("").is_empty());
    }

    #[test]
    fn markdown_export() {
        let c = Chat { id: new_id(), title: "T".into(), system: "S".into(), messages: vec![msg(Role::User, "hi")], ..Default::default() };
        let md = export_markdown(&c);
        assert!(md.starts_with("# T") && md.contains("## System") && md.contains("## User\n\nhi"));
    }

    #[test]
    fn personas_seed_and_builtin_rules() {
        let (d, s) = store();
        let list = s.personas();
        assert_eq!(list.len(), 4);
        assert!(list.iter().all(|p| p.builtin));
        assert!(d.path().join("personas.json").is_file());
        assert!(list.iter().any(|p| p.name == "翻訳 (日英)"));
        // deleting a built-in is refused
        assert_eq!(s.delete_persona("builtin-general").unwrap_err().0, StatusCode::FORBIDDEN);
        // editing a built-in creates a copy
        let copy = s.update_persona("builtin-general", &json!({"name": "mine"})).unwrap();
        assert!(!copy.builtin && copy.id != "builtin-general" && copy.name == "mine");
        let all = s.personas();
        assert_eq!(all.len(), 5);
        assert_eq!(all.iter().find(|p| p.id == "builtin-general").unwrap().name, "汎用アシスタント");
        // custom persona CRUD
        let p = s.create_persona(&json!({"name": "bot", "emoji": "🤖", "system": "be a bot", "params": {"top_p": 0.9}})).unwrap();
        assert!(valid_id(&p.id) && !p.builtin);
        let p2 = s.update_persona(&p.id, &json!({"description": "d"})).unwrap();
        assert_eq!((p2.id.as_str(), p2.description.as_str(), p2.name.as_str()), (p.id.as_str(), "d", "bot"));
        assert!(s.create_persona(&json!({"name": " "})).is_err());
        s.delete_persona(&p.id).unwrap();
        assert!(s.delete_persona(&p.id).is_err());
        assert!(s.update_persona("../x", &json!({})).is_err());
        // not re-seeded once the file exists
        assert_eq!(s.personas().len(), 5);
    }

    #[test]
    fn corrupt_personas_file_falls_back() {
        let (d, s) = store();
        std::fs::write(d.path().join("personas.json"), "garbage").unwrap();
        assert_eq!(s.personas().len(), 4);
        assert_eq!(std::fs::read_to_string(d.path().join("personas.json")).unwrap(), "garbage");
    }
}
