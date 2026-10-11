//! Minimal GGUF header reader. Only the metadata key/value section is parsed (no tensor
//! info). Large arrays such as the tokenizer vocabulary are skipped by seeking, never
//! materialised, and oversized strings are skipped the same way.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;

use crate::types::GgufInfo;

const MAGIC: &[u8; 4] = b"GGUF";
/// Strings longer than this are seeked over and stored as `Value::Other`.
const MAX_STRING: u64 = 1 << 20;
/// Metadata keys are short; anything longer means the header is corrupt.
const MAX_KEY: u64 = 1 << 16;
/// Smallest possible KV entry: 8-byte key length, 4-byte type, 1-byte value.
const MIN_KV_BYTES: u64 = 13;
/// Per-layer `*head_count_kv` arrays are kept only up to this many items.
const MAX_KEPT_ITEMS: u64 = 1 << 16;
/// Maximum array nesting accepted (real files use one level).
const MAX_DEPTH: u32 = 4;

/// llama.cpp `general.file_type` values -> names.
const FILE_TYPES: &[(u64, &str)] = &[
    (0, "F32"),
    (1, "F16"),
    (2, "Q4_0"),
    (3, "Q4_1"),
    (7, "Q8_0"),
    (8, "Q5_0"),
    (9, "Q5_1"),
    (10, "Q2_K"),
    (11, "Q3_K_S"),
    (12, "Q3_K_M"),
    (13, "Q3_K_L"),
    (14, "Q4_K_S"),
    (15, "Q4_K_M"),
    (16, "Q5_K_S"),
    (17, "Q5_K_M"),
    (18, "Q6_K"),
    (19, "IQ2_XXS"),
    (20, "IQ2_XS"),
    (21, "Q2_K_S"),
    (22, "IQ3_XS"),
    (23, "IQ3_XXS"),
    (24, "IQ1_S"),
    (25, "IQ4_NL"),
    (26, "IQ3_S"),
    (27, "IQ3_M"),
    (28, "IQ2_S"),
    (29, "IQ2_M"),
    (30, "IQ4_XS"),
    (31, "IQ1_M"),
    (32, "BF16"),
    (36, "TQ1_0"),
    (37, "TQ2_0"),
    (38, "MXFP4"),
];

/// Only the values the summary needs are kept; everything else is `Other`.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    UInt(u64),
    Int(i64),
    Str(String),
    /// A retained integer array (only ever built for `*head_count_kv`).
    List(Vec<Value>),
    /// Floats, bools, oversized strings and skipped arrays.
    Other,
}

impl Value {
    fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::UInt(v) => Some(v),
            Value::Int(v) => u64::try_from(v).ok(),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Byte width of a fixed-size scalar type, or `None` for strings, arrays and unknown types.
fn scalar_size(vtype: u32) -> Option<u64> {
    match vtype {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

/// Bounds-checked little-endian reader that tracks its offset and the file length, so
/// every skip is validated before seeking.
struct Reader {
    input: BufReader<File>,
    len: u64,
    pos: u64,
}

impl Reader {
    fn ensure(&self, n: u64) -> Result<()> {
        if n > self.len - self.pos {
            bail!("truncated GGUF: need {n} bytes at offset {}", self.pos);
        }
        Ok(())
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.ensure(N as u64)?;
        let mut buf = [0u8; N];
        self.input.read_exact(&mut buf)?;
        self.pos += N as u64;
        Ok(buf)
    }

    fn read_bytes(&mut self, n: u64) -> Result<Vec<u8>> {
        self.ensure(n)?;
        let mut buf = vec![0u8; n as usize];
        self.input.read_exact(&mut buf)?;
        self.pos += n;
        Ok(buf)
    }

    fn skip(&mut self, n: u64) -> Result<()> {
        self.ensure(n)?;
        self.input.seek(SeekFrom::Current(i64::try_from(n)?))?;
        self.pos += n;
        Ok(())
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn read_key(&mut self) -> Result<String> {
        let n = self.read_u64()?;
        if n > MAX_KEY {
            bail!("metadata key length {n} is implausible");
        }
        let bytes = self.read_bytes(n)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn read_string(&mut self) -> Result<Value> {
        let n = self.read_u64()?;
        if n > MAX_STRING {
            self.skip(n)?;
            return Ok(Value::Other);
        }
        let bytes = self.read_bytes(n)?;
        Ok(Value::Str(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn read_value(&mut self, vtype: u32, key: &str, depth: u32) -> Result<Value> {
        Ok(match vtype {
            0 => Value::UInt(u64::from(u8::from_le_bytes(self.take()?))),
            1 => Value::Int(i64::from(i8::from_le_bytes(self.take()?))),
            2 => Value::UInt(u64::from(u16::from_le_bytes(self.take()?))),
            3 => Value::Int(i64::from(i16::from_le_bytes(self.take()?))),
            4 => Value::UInt(u64::from(self.read_u32()?)),
            5 => Value::Int(i64::from(i32::from_le_bytes(self.take()?))),
            6 => {
                self.skip(4)?;
                Value::Other
            }
            7 => {
                self.skip(1)?;
                Value::Other
            }
            8 => self.read_string()?,
            9 => self.read_array(key, depth)?,
            10 => Value::UInt(self.read_u64()?),
            11 => Value::Int(i64::from_le_bytes(self.take()?)),
            12 => {
                self.skip(8)?;
                Value::Other
            }
            _ => bail!("unknown GGUF value type {vtype}"),
        })
    }

    /// Reads an array header and checks that `count` items can physically fit in the file.
    fn read_array_header(&mut self, depth: u32) -> Result<(u32, u64)> {
        if depth >= MAX_DEPTH {
            bail!("GGUF arrays nested deeper than {MAX_DEPTH}");
        }
        let itype = self.read_u32()?;
        let count = self.read_u64()?;
        let min_item = match itype {
            8 => 8,  // string: at least its u64 length
            9 => 12, // nested array: at least item type + count
            t => scalar_size(t).ok_or_else(|| anyhow!("unknown array item type {t}"))?,
        };
        if count > (self.len - self.pos) / min_item {
            bail!("array of {count} items does not fit in the file");
        }
        Ok((itype, count))
    }

    fn read_array(&mut self, key: &str, depth: u32) -> Result<Value> {
        let (itype, count) = self.read_array_header(depth)?;
        // Per-layer head counts (e.g. with GQA variants) are kept so the max can be taken.
        if key.ends_with("head_count_kv") && count <= MAX_KEPT_ITEMS && scalar_size(itype).is_some()
        {
            let mut items = Vec::with_capacity(count as usize);
            for _ in 0..count {
                items.push(self.read_value(itype, key, depth + 1)?);
            }
            return Ok(Value::List(items));
        }
        self.skip_items(itype, count, depth + 1)?;
        Ok(Value::Other)
    }

    /// Seeks over the items of an array whose header has already been read and validated.
    fn skip_items(&mut self, itype: u32, count: u64, depth: u32) -> Result<()> {
        if let Some(size) = scalar_size(itype) {
            let total = count.checked_mul(size).context("array too large")?;
            return self.skip(total);
        }
        for _ in 0..count {
            if itype == 8 {
                let n = self.read_u64()?;
                self.skip(n)?;
            } else {
                let (inner, n) = self.read_array_header(depth)?;
                self.skip_items(inner, n, depth + 1)?;
            }
        }
        Ok(())
    }
}

/// Parses the metadata section of a GGUF file.
fn parse(path: &Path) -> Result<HashMap<String, Value>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut r = Reader {
        input: BufReader::new(file),
        len,
        pos: 0,
    };

    if r.take::<4>()? != *MAGIC {
        bail!("not a GGUF file");
    }
    let version = r.read_u32()?;
    if !(1..=3).contains(&version) {
        bail!("unsupported GGUF version {version}");
    }
    // v1 stores the counts as u32; v2 and later use u64. The tensor count is not needed.
    let count_width = if version == 1 { 4 } else { 8 };
    r.skip(count_width)?;
    let kv_count = if version == 1 {
        u64::from(r.read_u32()?)
    } else {
        r.read_u64()?
    };
    if kv_count > (r.len - r.pos) / MIN_KV_BYTES {
        bail!("implausible metadata count {kv_count}");
    }

    let mut meta = HashMap::new();
    for _ in 0..kv_count {
        let key = r.read_key()?;
        let vtype = r.read_u32()?;
        let value = r
            .read_value(vtype, &key, 0)
            .with_context(|| format!("metadata key {key:?}"))?;
        meta.insert(key, value);
    }
    Ok(meta)
}

fn summarize(meta: &HashMap<String, Value>) -> GgufInfo {
    let arch = meta
        .get("general.architecture")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let arch_key = |suffix: &str| meta.get(&format!("{arch}.{suffix}"));
    let num = |suffix: &str| arch_key(suffix).and_then(Value::as_u64);
    let text = |key: &str| meta.get(key).and_then(Value::as_str).map(str::to_owned);
    let nonzero = |v: Option<u64>| v.filter(|&x| x != 0);

    let head_count = num("attention.head_count");
    let head_count_kv = match arch_key("attention.head_count_kv") {
        Some(Value::List(items)) => items.iter().filter_map(Value::as_u64).max(),
        Some(v) => v.as_u64(),
        None => None,
    };
    let embedding_length = num("embedding_length");
    let key_length = nonzero(num("attention.key_length")).or_else(|| {
        match (nonzero(embedding_length), nonzero(head_count)) {
            (Some(e), Some(h)) => Some(e / h),
            _ => None,
        }
    });
    let value_length = nonzero(num("attention.value_length")).or(key_length);
    let file_type = meta
        .get("general.file_type")
        .and_then(Value::as_u64)
        .map(|ft| {
            FILE_TYPES
                .iter()
                .find(|(id, _)| *id == ft)
                .map_or_else(|| ft.to_string(), |(_, name)| (*name).to_string())
        });

    GgufInfo {
        architecture: Some(arch.to_owned()),
        name: text("general.name"),
        size_label: text("general.size_label"),
        file_type,
        context_length: num("context_length"),
        block_count: num("block_count"),
        embedding_length,
        head_count,
        head_count_kv,
        key_length,
        value_length,
        expert_count: num("expert_count"),
        expert_used_count: num("expert_used_count"),
        has_chat_template: meta.contains_key("tokenizer.chat_template"),
        error: None,
    }
}

fn error_info(msg: &str) -> GgufInfo {
    GgufInfo {
        error: Some(msg.to_owned()),
        ..GgufInfo::default()
    }
}

/// Memo entry; valid only while the file's mtime and length are unchanged.
struct Memo {
    mtime: SystemTime,
    len: u64,
    info: GgufInfo,
}

static MEMO: LazyLock<Mutex<HashMap<PathBuf, Memo>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Read the header of `path` and summarise it. Never panics; errors go to `GgufInfo::error`.
///
/// Results are memoised per path and re-parsed when the file's mtime or length changes
/// (a download in progress changes both).
pub fn read_info(path: &Path) -> GgufInfo {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return error_info(&e.to_string()),
    };
    let stamp = meta.modified().ok().map(|mtime| (mtime, meta.len()));

    if let Some((mtime, len)) = stamp {
        if let Some(hit) = MEMO
            .lock()
            .get(path)
            .filter(|m| m.mtime == mtime && m.len == len)
        {
            return hit.info.clone();
        }
    }

    let info = match parse(path) {
        Ok(kv) => summarize(&kv),
        Err(e) => error_info(&format!("{e:#}")),
    };
    if let Some((mtime, len)) = stamp {
        MEMO.lock().insert(
            path.to_path_buf(),
            Memo {
                mtime,
                len,
                info: info.clone(),
            },
        );
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assembles a GGUF v3 byte stream from metadata entries.
    #[derive(Default)]
    struct Gguf {
        kvs: Vec<u8>,
        count: u64,
    }

    impl Gguf {
        fn entry(&mut self, key: &str, vtype: u32) -> &mut Self {
            self.kvs.extend((key.len() as u64).to_le_bytes());
            self.kvs.extend(key.as_bytes());
            self.kvs.extend(vtype.to_le_bytes());
            self.count += 1;
            self
        }

        fn raw(&mut self, bytes: &[u8]) -> &mut Self {
            self.kvs.extend(bytes);
            self
        }

        fn string(&mut self, key: &str, value: &str) -> &mut Self {
            self.entry(key, 8)
                .raw(&(value.len() as u64).to_le_bytes())
                .raw(value.as_bytes())
        }

        fn u32(&mut self, key: &str, value: u32) -> &mut Self {
            self.entry(key, 4).raw(&value.to_le_bytes())
        }

        fn u32_array(&mut self, key: &str, values: &[u32]) -> &mut Self {
            self.entry(key, 9)
                .raw(&4u32.to_le_bytes())
                .raw(&(values.len() as u64).to_le_bytes());
            for v in values {
                self.raw(&v.to_le_bytes());
            }
            self
        }

        fn string_array<S: AsRef<str>>(&mut self, key: &str, values: &[S]) -> &mut Self {
            self.entry(key, 9)
                .raw(&8u32.to_le_bytes())
                .raw(&(values.len() as u64).to_le_bytes());
            for v in values {
                let s = v.as_ref();
                self.raw(&(s.len() as u64).to_le_bytes()).raw(s.as_bytes());
            }
            self
        }

        /// An array of `count` zeroed fixed-size items of type `itype`.
        fn zero_array(&mut self, key: &str, itype: u32, count: u64, item_size: usize) -> &mut Self {
            self.entry(key, 9)
                .raw(&itype.to_le_bytes())
                .raw(&count.to_le_bytes());
            self.kvs
                .resize(self.kvs.len() + count as usize * item_size, 0);
            self
        }

        fn bytes(&self) -> Vec<u8> {
            let mut out = MAGIC.to_vec();
            out.extend(3u32.to_le_bytes());
            out.extend(0u64.to_le_bytes()); // tensor count
            out.extend(self.count.to_le_bytes());
            out.extend(&self.kvs);
            out
        }
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn info_of(bytes: &[u8]) -> GgufInfo {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_file(tmp.path(), "m.gguf", bytes);
        read_info(&path)
    }

    fn named(name: &str) -> Gguf {
        let mut g = Gguf::default();
        g.string("general.architecture", "llama")
            .string("general.name", name);
        g
    }

    #[test]
    fn summarises_metadata_and_skips_large_arrays() {
        let mut g = Gguf::default();
        g.string("general.architecture", "llama")
            .string("general.name", "Test Model")
            .string("general.size_label", "8B")
            .u32("general.file_type", 15)
            .u32("llama.context_length", 8192)
            .u32("llama.block_count", 4)
            .u32("llama.embedding_length", 4096)
            .u32("llama.attention.head_count", 32)
            .u32_array("llama.attention.head_count_kv", &[8, 16, 4, 8])
            .string_array(
                "tokenizer.ggml.tokens",
                &(0..150_000).map(|i| format!("tok{i}")).collect::<Vec<_>>(),
            )
            .zero_array("tokenizer.ggml.scores", 6, 150_000, 4)
            .string("tokenizer.huge_blob", &"x".repeat(2 << 20))
            .string(
                "tokenizer.chat_template",
                "{% for m in messages %}{% endfor %}",
            )
            .u32("llama.attention.value_length", 64);
        let tmp = tempfile::tempdir().unwrap();
        let path = write_file(tmp.path(), "model.gguf", &g.bytes());

        let info = read_info(&path);
        assert_eq!(info.error, None);
        assert_eq!(info.architecture.as_deref(), Some("llama"));
        assert_eq!(info.name.as_deref(), Some("Test Model"));
        assert_eq!(info.size_label.as_deref(), Some("8B"));
        assert_eq!(info.file_type.as_deref(), Some("Q4_K_M"));
        assert_eq!(info.context_length, Some(8192));
        assert_eq!(info.block_count, Some(4));
        assert_eq!(info.embedding_length, Some(4096));
        assert_eq!(info.head_count, Some(32));
        assert_eq!(info.head_count_kv, Some(16));
        assert_eq!(info.key_length, Some(128));
        assert_eq!(info.value_length, Some(64));
        assert_eq!(info.expert_count, None);
        assert_eq!(info.expert_used_count, None);
        assert!(info.has_chat_template);
    }

    #[test]
    fn scalar_head_count_kv_and_unknown_file_type() {
        let mut g = named("x");
        g.u32("general.file_type", 999)
            .u32("llama.attention.head_count_kv", 8);
        let info = info_of(&g.bytes());
        assert_eq!(info.file_type.as_deref(), Some("999"));
        assert_eq!(info.head_count_kv, Some(8));
        assert!(!info.has_chat_template);
    }

    #[test]
    fn missing_architecture_defaults_to_unknown() {
        let mut g = Gguf::default();
        g.u32("general.file_type", 1);
        let info = info_of(&g.bytes());
        assert_eq!(info.architecture.as_deref(), Some("unknown"));
        assert_eq!(info.file_type.as_deref(), Some("F16"));
        assert_eq!(info.key_length, None);
    }

    #[test]
    fn bad_files_report_errors_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let not_gguf = write_file(tmp.path(), "a.gguf", b"PK\x03\x04 zip data here");
        let info = read_info(&not_gguf);
        assert!(info.error.unwrap().contains("not a GGUF file"));
        assert_eq!(info.architecture, None);

        let empty = write_file(tmp.path(), "empty.gguf", b"");
        assert!(read_info(&empty).error.is_some());

        let mut bytes = named("cut off").bytes();
        bytes.truncate(bytes.len() - 3);
        let cut = write_file(tmp.path(), "cut.gguf", &bytes);
        assert!(read_info(&cut).error.is_some());

        assert!(read_info(&tmp.path().join("missing.gguf")).error.is_some());
    }

    #[test]
    fn hostile_sizes_are_rejected() {
        // String array claiming u64::MAX items.
        let mut g = Gguf::default();
        g.entry("tokenizer.tokens", 9)
            .raw(&8u32.to_le_bytes())
            .raw(&u64::MAX.to_le_bytes());
        assert!(info_of(&g.bytes()).error.is_some());

        // Key length far beyond anything real.
        let mut g = Gguf {
            count: 1,
            ..Gguf::default()
        };
        g.raw(&(1u64 << 40).to_le_bytes());
        assert!(info_of(&g.bytes()).error.is_some());

        // String value whose length runs past the end of the file.
        let mut g = Gguf::default();
        g.entry("general.name", 8).raw(&u64::MAX.to_le_bytes());
        assert!(info_of(&g.bytes()).error.is_some());

        // Per-layer head count array claiming more items than the file holds.
        let mut g = Gguf::default();
        g.entry("llama.attention.head_count_kv", 9)
            .raw(&4u32.to_le_bytes())
            .raw(&u64::MAX.to_le_bytes());
        assert!(info_of(&g.bytes()).error.is_some());

        // Absurd metadata count.
        let mut bytes = MAGIC.to_vec();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.extend(u64::MAX.to_le_bytes());
        assert!(info_of(&bytes).error.is_some());

        // Unknown value type.
        let mut g = Gguf::default();
        g.entry("general.name", 99);
        assert!(info_of(&g.bytes()).error.is_some());
    }

    #[test]
    fn results_are_memoised_and_refreshed_when_the_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_file(tmp.path(), "m.gguf", &named("First").bytes());
        assert_eq!(read_info(&path).name.as_deref(), Some("First"));
        assert_eq!(read_info(&path).name.as_deref(), Some("First"));

        std::fs::write(&path, named("A longer name").bytes()).unwrap();
        assert_eq!(read_info(&path).name.as_deref(), Some("A longer name"));
    }
}
