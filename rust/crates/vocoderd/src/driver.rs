//! The driver: the only code in vocoderd that performs I/O.
//!
//! Machines are pure (`handle(in) -> Vec<out>`); when a machine needs the
//! world touched it emits [`MachineOut::Realize`] with an effect id and awaits
//! [`MachineIn::EffectResult`] under that id. This module owns the other half
//! of that contract — executing the effect and shaping the answer.
//!
//! Keeping it in its own module (rather than inline in `main`) lets machine
//! unit tests drive a single machine through the real effect path without
//! building an HTTP host, which is what makes the purity refactor testable.

use vocoder_cordis::{EffectError, EffectResult, RealizeRequest};

#[cfg(test)]
use vocoder_cordis::{EffectId, MachineIn, MachineOut, PluginMachine};

/// Maximum effect round-trips allowed while answering one input.
///
/// A machine that keeps requesting effects without ever producing a reply is
/// a machine bug (a loop), and this bounds it the way
/// `MAX_DELIVERIES_PER_STEP` bounds the router's event fan-out. Sized well
/// above any legitimate chain: the deepest current path is session/create's
/// canonicalize → create-dir → write-generation.
pub const MAX_EFFECTS_PER_INPUT: usize = 256;

/// Map an `io::Error` to the cases machines distinguish between.
fn io_err(e: &std::io::Error) -> EffectError {
    if e.kind() == std::io::ErrorKind::NotFound {
        EffectError::NotFound
    } else {
        EffectError::Other(e.to_string())
    }
}

/// Perform one effect. Returns `None` for driver-local effects (logging,
/// socket writes, stream routing) that produce no machine answer — those are
/// terminal for the machine that emitted them.
///
/// Synchronous `std::fs` is deliberate: the effects are small, and it keeps
/// the machine contract pure (`handle` stays sync, no `async_trait` on
/// machines). Were these to become slow (network, subprocess), this would move
/// behind `spawn_blocking` without changing a single machine.
pub fn realize(request: RealizeRequest) -> Option<EffectResult> {
    Some(match request {
        // Driver-local: no answer goes back to the machine.
        RealizeRequest::Log { level, message } => {
            match level.as_str() {
                "error" => tracing::error!("{message}"),
                "warn" => tracing::warn!("{message}"),
                "debug" => tracing::debug!("{message}"),
                _ => tracing::info!("{message}"),
            }
            return None;
        }
        RealizeRequest::SendText { text } => {
            tracing::debug!("driver: SendText ({} bytes)", text.len());
            return None;
        }
        RealizeRequest::OpenStream {
            stream_id,
            endpoint,
            ..
        } => {
            tracing::debug!("driver: OpenStream {stream_id} -> {endpoint}");
            return None;
        }
        RealizeRequest::CancelStream { stream_id } => {
            tracing::debug!("driver: CancelStream {stream_id}");
            return None;
        }
        // Filesystem effects: answered back to the machine.
        RealizeRequest::ReadText { path } => match std::fs::read_to_string(&path) {
            Ok(text) => EffectResult::Text(text),
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::ReadBytes { path } => match std::fs::read(&path) {
            Ok(bytes) => EffectResult::Bytes(bytes),
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::WriteText { path, contents } => {
            match write_atomically(std::path::Path::new(&path), contents.as_bytes()) {
                Ok(()) => EffectResult::Done,
                Err(e) => EffectResult::Failed(io_err(&e)),
            }
        }
        RealizeRequest::WriteBytes { path, contents } => {
            match write_atomically(std::path::Path::new(&path), &contents) {
                Ok(()) => EffectResult::Done,
                Err(e) => EffectResult::Failed(io_err(&e)),
            }
        }
        RealizeRequest::CreateDirAll { path } => match std::fs::create_dir_all(&path) {
            Ok(()) => EffectResult::Done,
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::ReadRange {
            path,
            offset,
            limit,
        } => match read_range(std::path::Path::new(&path), offset, limit) {
            Ok((data, eof)) => EffectResult::Range { data, eof },
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::ListDirDetailed { path } => match list_dir_detailed(&path) {
            Ok(entries) => EffectResult::DirEntries(entries),
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::Stat { path } => match std::fs::metadata(&path) {
            // Existence-then-resolve, so a dangling symlink reports NotFound
            // rather than resolving to a path that no longer exists. Every
            // call site wants "a real directory", not "a name that resolves".
            Ok(meta) => match std::fs::canonicalize(&path) {
                Ok(canonical) => EffectResult::Stat {
                    canonical: canonical.to_string_lossy().to_string(),
                    is_dir: meta.is_dir(),
                    bytes: meta.len(),
                    version: version_token(&meta),
                },
                Err(e) => EffectResult::Failed(io_err(&e)),
            },
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::ListDir { path } => match std::fs::read_dir(&path) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect();
                names.sort();
                EffectResult::Entries(names)
            }
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::ListTree { path } => match list_tree(std::path::Path::new(&path)) {
            Ok(paths) => EffectResult::Paths(paths),
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
    })
}

/// Every file beneath `root`, as absolute paths, sorted.
///
/// Sorted output is load-bearing: the session machine derives "latest
/// generation" from the lexicographically last generation filename, so a
/// nondeterministic walk would make `session/list` order-dependent.
fn list_tree(root: &std::path::Path) -> std::io::Result<Vec<String>> {
    // A missing root is an empty tree, not an error: the host's sessions
    // directory does not exist until the first session is created.
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                out.push(path.to_string_lossy().to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A change token for a file, derived from size + mtime.
///
/// Not a content hash: the file API's contract is "tell me if it changed since
/// this token", and mtime is what the filesystem gives cheaply. Equal tokens
/// therefore mean "probably unchanged", which is the intended strength.
fn version_token(meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}", mtime, meta.len())
}

/// Read `limit` bytes from `offset` (to EOF when `limit` is `None`).
///
/// Returns `(data, eof)`. An offset past EOF is an empty read at EOF, not an
/// error: a client paging a truncated file should see the end, not a fault.
fn read_range(
    path: &std::path::Path,
    offset: u64,
    limit: Option<u64>,
) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let total = f.metadata()?.len();
    if offset >= total {
        return Ok((Vec::new(), true));
    }
    f.seek(SeekFrom::Start(offset))?;
    let want = match limit {
        Some(n) => (total - offset).min(n),
        None => total - offset,
    };
    let mut buf = vec![0u8; want as usize];
    f.read_exact(&mut buf)?;
    Ok((buf, offset + want >= total))
}

/// Immediate entries of `dir` with kind and size, sorted by name.
fn list_dir_detailed(path: &str) -> std::io::Result<Vec<vocoder_cordis::DirEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        // `file_type` does not follow symlinks, so a link reports as a link
        // rather than as whatever it points at — which is what a file browser
        // must show.
        let ft = entry.file_type()?;
        let kind = if ft.is_dir() {
            "dir"
        } else if ft.is_symlink() {
            "symlink"
        } else {
            "file"
        };
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(vocoder_cordis::DirEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            kind: kind.to_string(),
            bytes,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Write bytes via temp-file + rename, so a reader never observes a partial
/// file and a crash never leaves a truncated one. Parent directories are
/// created as needed.
///
/// Session generations depend on this: they are immutable once published, and
/// `vocoder-session`'s committed-artifact check treats any existing destination
/// as frozen.
pub fn write_atomically(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Drive one machine through the effect loop to quiescence.
///
/// Returns the machine's terminal outputs — crucially, outputs it produced
/// *after* the last effect answer, so a caller whose operation suspends on I/O
/// still sees its final reply.
///
/// Test-only: the live host needs [`AppState::pump`], which additionally
/// routes stream frames and dispatches events to other machines. Keeping this
/// separate is what lets a machine be tested through the real effect path
/// without standing up an HTTP host.
#[cfg(test)]
pub fn drive(
    machine: &mut dyn PluginMachine<In = MachineIn, Out = MachineOut>,
    input: MachineIn,
) -> Vec<MachineOut> {
    let mut terminal: Vec<MachineOut> = Vec::new();
    let mut pending = Some(input);
    for _ in 0..MAX_EFFECTS_PER_INPUT {
        let Some(next) = pending.take() else { break };
        let outs = machine.handle(next);

        let mut effects: Vec<(EffectId, RealizeRequest)> = Vec::new();
        for out in outs {
            match out {
                MachineOut::Realize { id, request } => effects.push((id, request)),
                other => terminal.push(other),
            }
        }
        if effects.is_empty() {
            break;
        }
        // A machine that awaits several effects issues them one at a time; at
        // most one is awaited per turn, and any others were fire-and-forget.
        let (id, request) = effects.remove(0);
        let result = realize(request).unwrap_or(EffectResult::Done);
        pending = Some(MachineIn::EffectResult { id, result });
    }
    terminal
}
