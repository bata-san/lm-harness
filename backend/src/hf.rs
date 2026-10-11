//! Hugging Face Hub API client (model search, file listing, resolve URLs).
//!
//! Two process-wide `reqwest::Client`s are shared by everything that talks to the Hub:
//!
//! * [`api_client`] — metadata calls (search / tree). HTTP/2 allowed, connection pooling.
//! * [`download_client`] — bulk transfers. HTTP/1.1 only on purpose: with HTTP/2 every Range
//!   segment of a file would be multiplexed over a *single* TCP connection, which defeats the
//!   point of segmented downloads (the HF/Xet CDN throttles per connection).
//!
//! The auth token and endpoint are read from the live [`Settings`] on every call, so changing
//! them in the UI takes effect immediately without rebuilding the clients.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_RANGE, LINK, RANGE};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::config::Settings;

pub const USER_AGENT: &str = "llm-harness/0.2";

static API: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .expect("build HF API client")
});

static DOWNLOAD: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .user_agent(USER_AGENT)
        .http1_only()
        .connect_timeout(Duration::from_secs(15))
        // A stalled connection (no bytes for a minute) errors out and is retried by the caller.
        .read_timeout(Duration::from_secs(60))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(32)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .expect("build HF download client")
});

/// Shared client for Hub metadata requests.
pub fn api_client() -> &'static Client {
    &API
}

/// Shared client for bulk (range) downloads: HTTP/1.1, follows redirects (HF -> CDN).
pub fn download_client() -> &'static Client {
    &DOWNLOAD
}

/// Attach `Authorization: Bearer <hf_token>` when a token is configured. reqwest drops the
/// header again on cross-host redirects, so the token never reaches the CDN.
pub fn authorize(rb: RequestBuilder, settings: &Settings) -> RequestBuilder {
    let token = settings.hf_token.trim();
    if token.is_empty() {
        rb
    } else {
        rb.bearer_auth(token)
    }
}

fn endpoint(settings: &Settings) -> &str {
    settings.hf_endpoint.trim_end_matches('/')
}

/// Percent-encode one URL path segment (RFC 3986 unreserved characters are kept).
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Encode a `/`-separated path, keeping the separators.
fn encode_path(p: &str) -> String {
    p.split('/').map(pct_encode).collect::<Vec<_>>().join("/")
}

/// `{endpoint}/{repo}/resolve/{revision}/{path}` (revision `/` is encoded as `%2F`).
pub fn resolve_url(endpoint: &str, repo: &str, path: &str, revision: &str) -> String {
    format!("{}/{}/resolve/{}/{}", endpoint.trim_end_matches('/'), encode_path(repo), pct_encode(revision), encode_path(path))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HfRepo {
    pub id: String,
    pub downloads: u64,
    pub likes: u64,
    pub updated: Option<String>,
    pub pipeline_tag: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HfFile {
    pub path: String,
    pub size: u64,
    pub quant: Option<String>,
    pub split: bool,
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
struct RawRepo {
    id: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    likes: u64,
    #[serde(rename = "lastModified")]
    last_modified: Option<String>,
    pipeline_tag: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct RawLfs {
    oid: Option<String>,
    size: Option<u64>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    size: Option<u64>,
    lfs: Option<RawLfs>,
}

static QUANT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(IQ\d_[A-Z]+|Q\d_K(?:_[SML]|_XL)?|Q\d_\d|Q\d_K|BF16|F16|F32|MXFP4)").expect("quant regex"));
static SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-(\d{5})-of-(\d{5})\.gguf$").expect("split regex"));

/// Quantisation tag from a file name (`model-Q4_K_M.gguf` -> `Q4_K_M`).
pub fn quant_of(path: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    QUANT.captures(name).map(|c| c[1].to_uppercase())
}

pub fn is_split(path: &str) -> bool {
    SPLIT.is_match(path)
}

/// Turn a non-2xx response into a readable error (HF puts a message in `{"error": ...}`).
async fn check(resp: Response) -> anyhow::Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(200).collect());
    let hint = if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
        " (gated repo なら HF token を設定)"
    } else {
        ""
    };
    if detail.trim().is_empty() {
        bail!("HTTP {}{hint}", status.as_u16());
    }
    bail!("HTTP {}{hint}: {}", status.as_u16(), detail.trim());
}

/// `GET /api/models?search=&sort=downloads&direction=-1&limit=&filter=gguf`
pub async fn search(settings: &Settings, query: &str, gguf_only: bool, limit: u32) -> anyhow::Result<Vec<HfRepo>> {
    let mut params: Vec<(&str, String)> = vec![
        ("search", query.to_string()),
        ("sort", "downloads".into()),
        ("direction", "-1".into()),
        ("limit", limit.to_string()),
    ];
    if gguf_only {
        params.push(("filter", "gguf".into()));
    }
    let rb = authorize(api_client().get(format!("{}/api/models", endpoint(settings))).query(&params), settings);
    let resp = check(rb.timeout(Duration::from_secs(20)).send().await.context("Hugging Face に接続できません")?).await?;
    let raw: Vec<RawRepo> = resp.json().await.context("Hugging Face の応答を解釈できません")?;
    Ok(raw
        .into_iter()
        .filter_map(|m| {
            Some(HfRepo {
                id: m.id.or(m.model_id)?,
                downloads: m.downloads,
                likes: m.likes,
                updated: m.last_modified,
                pipeline_tag: m.pipeline_tag,
                tags: m.tags.into_iter().take(12).collect(),
            })
        })
        .collect())
}

fn parse_entries(entries: Vec<RawEntry>) -> Vec<HfFile> {
    entries
        .into_iter()
        .filter(|e| e.kind == "file")
        .map(|e| {
            let size = e.lfs.as_ref().and_then(|l| l.size).or(e.size).unwrap_or(0);
            HfFile {
                quant: quant_of(&e.path),
                split: is_split(&e.path),
                sha256: e.lfs.and_then(|l| l.oid),
                path: e.path,
                size,
            }
        })
        .collect()
}

/// Extract the `rel="next"` URL from a `Link` header (HF paginates large trees).
fn next_link(headers: &HeaderMap<HeaderValue>) -> Option<String> {
    let link = headers.get(LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, rest) = part.split_once(';')?;
        if !rest.contains("rel=\"next\"") {
            return None;
        }
        Some(url.trim().trim_start_matches('<').trim_end_matches('>').to_string())
    })
}

/// `GET /api/models/{repo}/tree/{rev}?recursive=true` (follows pagination).
pub async fn files(settings: &Settings, repo: &str, revision: &str) -> anyhow::Result<Vec<HfFile>> {
    let mut url = format!("{}/api/models/{}/tree/{}?recursive=true", endpoint(settings), encode_path(repo), pct_encode(revision));
    let mut out = Vec::new();
    for _ in 0..100 {
        let rb = authorize(api_client().get(&url), settings).timeout(Duration::from_secs(30));
        let resp = check(rb.send().await.context("Hugging Face に接続できません")?).await?;
        let next = next_link(resp.headers());
        let entries: Vec<RawEntry> = resp.json().await.context("Hugging Face の応答を解釈できません")?;
        out.extend(parse_entries(entries));
        match next {
            Some(n) => url = n,
            None => break,
        }
    }
    Ok(out)
}

/// Size in bytes of a remote file, via a `Range: bytes=0-0` probe (`Content-Range: bytes 0-0/TOTAL`).
pub async fn remote_size(settings: &Settings, url: &str) -> anyhow::Result<u64> {
    let rb = authorize(download_client().get(url).header(RANGE, "bytes=0-0"), settings);
    let resp = rb.timeout(Duration::from_secs(30)).send().await.context("サイズを取得できません")?;
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403 | 404) {
        bail!("HTTP {} (gated repo なら HF token を設定)", status.as_u16());
    }
    if !status.is_success() {
        bail!("HTTP {} (サイズ取得)", status.as_u16());
    }
    if status == StatusCode::PARTIAL_CONTENT {
        if let Some(total) = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse::<u64>().ok())
        {
            return Ok(total);
        }
    }
    resp.content_length().ok_or_else(|| anyhow!("サイズを取得できません (Content-Length なし)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quant_detection() {
        assert_eq!(quant_of("Qwen3-30B-A3B-Q4_K_M.gguf").as_deref(), Some("Q4_K_M"));
        assert_eq!(quant_of("sub/dir/model-iq4_xs.gguf").as_deref(), Some("IQ4_XS"));
        assert_eq!(quant_of("model-BF16-00001-of-00003.gguf").as_deref(), Some("BF16"));
        assert_eq!(quant_of("model-Q8_0.gguf").as_deref(), Some("Q8_0"));
        assert_eq!(quant_of("README.md"), None);
        assert!(is_split("m-00001-of-00003.gguf"));
        assert!(!is_split("m-Q4_K_M.gguf"));
    }

    #[test]
    fn resolve_url_encodes() {
        assert_eq!(
            resolve_url("https://huggingface.co/", "org/repo", "dir/a b#1.gguf", "refs/pr/1"),
            "https://huggingface.co/org/repo/resolve/refs%2Fpr%2F1/dir/a%20b%231.gguf"
        );
    }

    #[test]
    fn tree_entries() {
        let json = r#"[
          {"type":"directory","path":"sub"},
          {"type":"file","path":"m-Q4_K_M.gguf","size":134,"lfs":{"oid":"abc","size":4000,"pointerSize":134}},
          {"type":"file","path":"config.json","size":12}
        ]"#;
        let f = parse_entries(serde_json::from_str(json).unwrap());
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].size, 4000);
        assert_eq!(f[0].sha256.as_deref(), Some("abc"));
        assert_eq!(f[0].quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(f[1].size, 12);
        assert_eq!(f[1].sha256, None);
    }

    #[test]
    fn link_header() {
        let mut h = HeaderMap::new();
        h.insert(LINK, HeaderValue::from_static("<https://hf.co/api/x?cursor=1>; rel=\"next\", <https://hf.co/api/x>; rel=\"prev\""));
        assert_eq!(next_link(&h).as_deref(), Some("https://hf.co/api/x?cursor=1"));
        assert_eq!(next_link(&HeaderMap::new()), None);
    }
}
