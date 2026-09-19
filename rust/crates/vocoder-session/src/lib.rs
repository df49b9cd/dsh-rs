//! vocoder-session — the durable session log plane.
//!
//! File rules mirror `dsh/packages/session/session-persistence-jsonl/src/format.ts`:
//!   - v0: `session.jsonl` | `session.jsonl.zstd`
//!   - vN≥1: `session.vN.jsonl[.zstd]`
//!   - First JSONL row is the session header (`{"type":"session","version":N,...}`).
//!   - Committed generations are never renamed, replaced, or deleted.
//!   - `open` selects the numerically-highest canonical generation and
//!     publishes a version-named successor (vN → vN+1) via exclusive
//!     create/replace, never editing the predecessor in place.
//!
//! Adjacent migrations each supply exactly one `vN → vN+1` step; composing
//! steps walks the chain from any supported source to the current format.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Generation naming
// ---------------------------------------------------------------------------

/// Canonical generation filename (without extension).
pub fn generation_basename(version: u32) -> String {
    if version == 0 {
        "session".to_string()
    } else {
        format!("session.v{version}")
    }
}

/// All canononical names (with allowed extensions), preferred zstd-first.
pub fn generation_filename(version: u32, compressed: bool) -> String {
    let base = generation_basename(version);
    if compressed {
        format!("{base}.jsonl.zstd")
    } else {
        format!("{base}.jsonl")
    }
}

/// Parse a directory entry as a canonical generation name; returns version.
/// Rejects temp, uppercase, leading-zero, and non-canonical names.
pub fn parse_generation_filename(name: &str) -> Option<u32> {
    let stem = name
        .strip_suffix(".jsonl.zstd")
        .or_else(|| name.strip_suffix(".jsonl"))?;
    if stem == "session" {
        return Some(0);
    }
    let rest = stem.strip_prefix("session.v")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Reject leading zeros ("v03") — the canonical form is minimal digits.
    if rest.len() > 1 && rest.starts_with('0') {
        return None;
    }
    rest.parse().ok()
}

/// Enumerate committed generations in a session directory.
pub fn list_generations(dir: &Path) -> std::io::Result<Vec<u32>> {
    let mut versions = BTreeSet::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(v) = parse_generation_filename(&entry.file_name().to_string_lossy()) {
            versions.insert(v);
        }
    }
    Ok(versions.into_iter().collect())
}

/// Highest canonical version present; None when the directory hosts none.
pub fn latest_generation(dir: &Path) -> std::io::Result<Option<u32>> {
    Ok(list_generations(dir)?.into_iter().max())
}

/// Path of a committed generation; prefers the zstd variant when both exist.
pub fn generation_path(dir: &Path, version: u32) -> Option<PathBuf> {
    for compressed in [true, false] {
        let p = dir.join(generation_filename(version, compressed));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// The first JSONL row of any generation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionHeader {
    /// Literal "session".
    #[serde(rename = "type")]
    pub row_type: String,
    pub version: u32,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

/// Read just the header row from a generation file (zstd-aware).
pub fn read_header(path: &Path) -> Result<SessionHeader, SessionError> {
    let mut rdr = open_reader(path)?;
    let mut line = String::new();
    use std::io::BufRead;
    let n = rdr.read_line(&mut line)?;
    if n == 0 {
        return Err(SessionError::Empty);
    }
    let header: SessionHeader =
        serde_json::from_str(&line).map_err(|e| SessionError::BadJson { line: 1, source: e })?;
    Ok(header)
}

// ---------------------------------------------------------------------------
// Codec — read/write JSONL[.zstd]
// ---------------------------------------------------------------------------

/// Open a BufRead for either JSONL or JSONL.zstd.
fn open_reader(path: &Path) -> Result<Box<dyn std::io::BufRead>, SessionError> {
    use std::io::BufReader;
    let file = std::fs::File::open(path)?;
    if path.extension().map(|e| e == "zstd").unwrap_or(false) {
        let dec = zstd::stream::read::Decoder::new(file)
            .map_err(|e| SessionError::Zstd(e.to_string()))?;
        Ok(Box::new(BufReader::new(dec)))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

/// Read a whole generation into JSON rows (header first).
pub fn read_generation(path: &Path) -> Result<Vec<serde_json::Value>, SessionError> {
    use std::io::BufRead;
    let mut rows = Vec::new();
    let mut rdr = open_reader(path)?;
    let mut line = String::new();
    let mut line_no = 0usize;
    loop {
        line.clear();
        let n = rdr.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(trimmed).map_err(|e| SessionError::BadJson {
                line: line_no,
                source: e,
            })?,
        );
    }
    Ok(rows)
}

/// Serialize one row exactly as dsh writes it: JSON with integral floats
/// rendered without a fraction (e.g. 123 not 123.0). dsh parses numbers with
/// JSON.parse so spellings are semantically equal, but byte parity keeps
/// snapshot diffs clean.
pub fn row_to_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64()
                && n.is_f64()
                && f.fract() == 0.0
                && f.abs() < 9.007_199_254_740_992e15
            {
                return format!("{f:.0}");
            }
            n.to_string()
        }
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(row_to_json).collect();
            format!("[{}]", parts.join(","))
        }
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).expect("map key"),
                        row_to_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        other => serde_json::to_string(other).expect("JSON value serializes"),
    }
}

/// Encode one generation to bytes, without touching the filesystem.
///
/// This is the pure half of [`write_generation`]: framing and (optional) zstd
/// compression are computation, not I/O, so a Sans-I/O machine can produce the
/// exact bytes and let the driver perform the write.
pub fn encode_generation(
    rows: &[serde_json::Value],
    compress: bool,
) -> Result<Vec<u8>, SessionError> {
    use std::io::Write;
    if compress {
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
        for row in rows {
            enc.write_all(row_to_json(row).as_bytes())?;
            enc.write_all(b"\n")?;
        }
        Ok(enc.finish()?)
    } else {
        let mut out = Vec::new();
        for row in rows {
            out.write_all(row_to_json(row).as_bytes())?;
            out.write_all(b"\n")?;
        }
        Ok(out)
    }
}

/// Decode generation bytes into JSON rows (header first).
///
/// Pure counterpart to [`encode_generation`], zstd-aware. The machine has the
/// driver read the file and calls this on the bytes.
pub fn decode_generation(
    bytes: &[u8],
    compressed: bool,
) -> Result<Vec<serde_json::Value>, SessionError> {
    let raw: Vec<u8> = if compressed {
        zstd::stream::decode_all(bytes).map_err(|e| SessionError::Zstd(e.to_string()))?
    } else {
        bytes.to_vec()
    };
    let text = String::from_utf8(raw).map_err(|e| SessionError::Zstd(e.to_string()))?;
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(trimmed).map_err(|e| SessionError::BadJson {
                line: i + 1,
                source: e,
            })?,
        );
    }
    Ok(rows)
}

/// Parse just the header row from generation bytes.
pub fn decode_header(bytes: &[u8], compressed: bool) -> Result<SessionHeader, SessionError> {
    let rows = decode_generation(bytes, compressed)?;
    let first = rows.first().ok_or(SessionError::Empty)?;
    serde_json::from_value(first.clone()).map_err(|e| SessionError::BadJson { line: 1, source: e })
}

/// Write one generation atomically (tmp + rename) into `dir`.
///
/// Standalone convenience for callers that are *not* Sans-I/O machines
/// (tests, interop harness). A machine should emit a `WriteText` effect
/// instead, whose driver implementation is atomic for the same reason.
pub fn write_generation(
    dir: &Path,
    rows: &[serde_json::Value],
    version: u32,
    compress: bool,
) -> Result<PathBuf, SessionError> {
    let target = dir.join(generation_filename(version, compress));
    // Never touch a committed generation.
    if target.exists() {
        return Err(SessionError::Committed(target));
    }
    let tmp = dir.join(format!(
        ".{}.tmp",
        target.file_name().unwrap().to_string_lossy()
    ));
    {
        use std::io::Write;
        let mut out = std::fs::File::create(&tmp)?;
        out.write_all(&encode_generation(rows, compress)?)?;
        out.flush()?;
    }
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}

// ---------------------------------------------------------------------------
// Migration chain (adjacent vN→vN+1 steps)
// ---------------------------------------------------------------------------

/// A single adjacent migration step, e.g. v2→v3's payload-validation+provenance change.
pub trait MigrationStep: Send + Sync {
    /// Source version (vN).
    fn from(&self) -> u32;
    /// Target version (from+1).
    fn to(&self) -> u32 {
        self.from() + 1
    }
    /// Transform rows of the source generation into rows of the target.
    fn migrate(&self, rows: Vec<serde_json::Value>)
    -> Result<Vec<serde_json::Value>, SessionError>;
}

/// Compose registered steps to lift `rows` from `from` to `to` (must equal
/// from+steps.len()); order matters, gaps are errors.
pub fn compose(
    steps: &[&dyn MigrationStep],
    from: u32,
    to: u32,
    rows: Vec<serde_json::Value>,
) -> Result<Vec<serde_json::Value>, SessionError> {
    let mut current = from;
    let mut rows = rows;
    while current < to {
        let step = steps
            .iter()
            .find(|s| s.from() == current)
            .ok_or(SessionError::NoMigration {
                from: current,
                to: current + 1,
            })?;
        rows = step.migrate(rows)?;
        current += 1;
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("generation is empty")]
    Empty,
    #[error("invalid JSON at line {line}: {source}")]
    BadJson {
        line: usize,
        source: serde_json::Error,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("zstd: {0}")]
    Zstd(String),
    #[error("generation path is committed and must never be modified: {0}")]
    Committed(PathBuf),
    #[error("no adjacent migration step for v{from}→v{to}")]
    NoMigration { from: u32, to: u32 },
}

impl From<serde_json::Error> for SessionError {
    fn from(e: serde_json::Error) -> Self {
        SessionError::BadJson { line: 0, source: e }
    }
}
