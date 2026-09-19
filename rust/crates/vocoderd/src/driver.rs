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
///
/// `NotFound` and `Exists` are separated because callers branch on them:
/// "absent" is a different answer from "occupied" for the directory picker
/// (`directory-picker/exists`) and for preset authoring (`agent-preset/invalid`),
/// and folding either into a generic failure would lose the wire code.
fn io_err(e: &std::io::Error) -> EffectError {
    match e.kind() {
        std::io::ErrorKind::NotFound => EffectError::NotFound,
        std::io::ErrorKind::AlreadyExists => EffectError::Exists,
        _ => EffectError::Other(e.to_string()),
    }
}

/// Perform one effect, forwarding a streaming effect's intermediate bytes to
/// `sink` as they arrive.
///
/// The sink is called *during* the effect, on this thread, with however many
/// bytes one read returned — so a streaming caller must be able to accept a
/// chunk at any byte boundary, including one that splits a UTF-8 character or
/// an SSE frame in half. Both decoders downstream work on bytes or accumulate,
/// so neither cares where the reads fell.
///
/// A sink that is never called means the effect had nothing to stream (every
/// non-streaming effect), so a caller with nowhere to put progress passes
/// `&mut |_| {}` and is unaffected.
///
/// Returns `None` for driver-local effects (logging, socket writes, stream
/// routing) that produce no machine answer — those are terminal for the machine
/// that emitted them.
///
/// Synchronous `std::fs` is deliberate: the effects are small, and it keeps
/// the machine contract pure (`handle` stays sync, no `async_trait` on
/// machines). Were these to become slow (network, subprocess), this would move
/// behind `spawn_blocking` without changing a single machine.
pub fn realize_with(
    request: RealizeRequest,
    sink: &mut dyn FnMut(Vec<u8>),
) -> Option<EffectResult> {
    // A `canned://` provider route is answered from the file it names rather
    // than the network, so a whole turn is testable hermetically. It is handled
    // here, in front of the real effect, rather than in a test-local copy of the
    // loop: the copy was free to skip the chunk deliveries, which would have
    // made every streaming assertion vacuous.
    #[cfg(test)]
    if let RealizeRequest::FetchJson { url, .. } | RealizeRequest::FetchStream { url, .. } =
        &request
        && let Some(body) = canned_body(url)
    {
        // A `canned-seq://` route answers with a *different* body per call, which
        // is what makes a multi-model-call turn testable: a tool-calling turn's
        // second call is made because the first called a tool, so it cannot
        // answer identically. A fresh directory per test keeps the counter
        // process-global but per-test.
        let _ = &body;
        // Streamed in slices, not handed over whole: a canned body that skipped
        // the sink would leave the incremental path untested for every
        // non-live test, which is the same vacuity one level down.
        for piece in body.as_bytes().chunks(64) {
            sink(piece.to_vec());
        }
        return Some(EffectResult::HttpResponse { status: 200, body });
    }
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
        // Non-recursive on purpose: the caller is browsing a directory it can
        // already see, so a missing parent is a real failure, not a level to
        // invent — and an occupied target must reach the caller as `Exists`
        // rather than being swallowed into success.
        RealizeRequest::CreateDir { path } => match std::fs::create_dir(&path) {
            Ok(()) => EffectResult::Done,
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::RemoveDirAll { path } => match std::fs::remove_dir_all(&path) {
            // Absent is the same outcome as removed: the caller asked for the
            // tree not to be there, and it is not.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => EffectResult::Done,
            Ok(()) => EffectResult::Done,
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::RemoveFile { path } => match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => EffectResult::Done,
            Ok(()) => EffectResult::Done,
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::CopyTree { from, to } => {
            match copy_tree(std::path::Path::new(&from), std::path::Path::new(&to)) {
                Ok(()) => EffectResult::Done,
                Err(e) => EffectResult::Failed(io_err(&e)),
            }
        }
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
        // The one effect that leaves this process. A non-2xx status is *not* a
        // failure here: provider error bodies carry the real reason, and the
        // machine that knows the dialect is the one that should read it.
        RealizeRequest::FetchJson { url, headers, body } => match fetch_json(&url, &headers, &body)
        {
            Ok((status, text)) => EffectResult::HttpResponse { status, body: text },
            Err(e) => EffectResult::Failed(EffectError::Other(e)),
        },
        // Same request, read as it arrives. The body is returned as well, so a
        // machine may stream *and* keep the whole thing; the agent does both
        // (chunks for display, body for the durable record).
        RealizeRequest::FetchStream { url, headers, body } => {
            match fetch_streaming(&url, &headers, &body, sink) {
                Ok((status, text)) => EffectResult::HttpResponse { status, body: text },
                Err(e) => EffectResult::Failed(EffectError::Other(e)),
            }
        }
    })
}

/// The canned body a `canned://<dir>` route names, if any.
///
/// `ProviderConfig::url` appends the dialect's own path, so the directory has to
/// be recovered by stripping it back off — stripping one component would leave
/// `/chat` on the end and look in a directory that does not exist.
#[cfg(test)]
fn canned_body(url: &str) -> Option<String> {
    if let Some(dir) = url.strip_prefix("canned-seq://") {
        let mut dir = dir.to_string();
        for suffix in ["/chat/completions", "/responses", "/messages"] {
            if let Some(stripped) = dir.strip_suffix(suffix) {
                dir = stripped.to_string();
                break;
            }
        }
        // The call number is process-global and keyed by directory, because the
        // driver is stateless and a route cannot carry a counter.
        static CALLS: std::sync::Mutex<std::collections::BTreeMap<String, usize>> =
            std::sync::Mutex::new(std::collections::BTreeMap::new());
        let mut calls = CALLS.lock().unwrap();
        let n = calls.entry(dir.clone()).or_insert(0);
        let path = format!("{dir}/body-{n}.txt");
        *n += 1;
        return std::fs::read_to_string(path).ok();
    }
    let dir = url.strip_prefix("canned://")?;
    let mut dir = dir.to_string();
    for suffix in ["/chat/completions", "/responses", "/messages"] {
        if let Some(stripped) = dir.strip_suffix(suffix) {
            dir = stripped.to_string();
            break;
        }
    }
    std::fs::read_to_string(format!("{dir}/body.txt")).ok()
}

/// Per-request timeout for a provider call.
///
/// A model call is slow by nature — reasoning models can stream for minutes —
/// so this bounds a *stalled* connection rather than a long one. ureq's
/// timeout applies per read/write operation, not to the whole transfer, which
/// is exactly the wanted semantics.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// POST a JSON body and return `(status, body_text)`.
///
/// Errors are reserved for transport failures and for bodies that are not text.
/// An HTTP error *status* is returned as data, because the provider dialects
/// put a structured reason in the body and only the machine knows how to read
/// it; collapsing it here would turn "rate limited, retry in 30s" into an
/// opaque failure.
///
/// `http_status_as_error(false)` is load-bearing rather than stylistic: with
/// ureq's default, a non-2xx is an `Error::StatusCode` that **discards the
/// body** (probed against a local 400, not assumed), which is precisely the
/// detail a provider's error message lives in.
fn fetch_json(
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<(u16, String), String> {
    let response = post(url, headers, body)?;
    let status = response.status().as_u16();
    response
        .into_body()
        .read_to_string()
        .map(|text| (status, text))
        .map_err(|e| format!("read response body: {e}"))
}

/// Byte size of one read from a streaming body.
///
/// The body is read in fixed slices rather than with `read_to_end` so each
/// slice can be handed to the sink the moment it lands. The size is a
/// throughput/latency knob and nothing more: correctness never depends on where
/// a read boundary falls, because the sink accepts an arbitrary byte boundary
/// and the SSE decoder downstream is incremental over bytes.
const STREAM_READ_CHUNK: usize = 8 * 1024;

/// POST a JSON body and read the response incrementally, handing each slice to
/// `sink` as it arrives. Returns `(status, whole_body)`.
///
/// The body is accumulated as well as streamed, because both consumers need it:
/// the sink renders the model's text live, and the returned body is what the
/// session log records. Dropping the accumulation to "save" the copy would mean
/// the durable record had to be rebuilt from the display feed, which is exactly
/// the coupling the markdown module exists to prevent.
///
/// A read error mid-stream is a failure, not a truncated success: a provider
/// stream that stopped early must not be recorded as a complete answer.
fn fetch_streaming(
    url: &str,
    headers: &[(String, String)],
    body: &str,
    sink: &mut dyn FnMut(Vec<u8>),
) -> Result<(u16, String), String> {
    use std::io::Read;
    let response = post(url, headers, body)?;
    let status = response.status().as_u16();
    let mut reader = response.into_body().into_reader();
    let mut buf = vec![0u8; STREAM_READ_CHUNK];
    let mut all: Vec<u8> = Vec::new();
    loop {
        match reader.read(&mut buf) {
            // EOF.
            Ok(0) => break,
            Ok(n) => {
                let slice = &buf[..n];
                sink(slice.to_vec());
                all.extend_from_slice(slice);
            }
            Err(e) => return Err(format!("read response body: {e}")),
        }
    }
    // Lossy on purpose, and deliberately so: a provider that emits invalid
    // UTF-8 mid-stream should cost the caller that character, not the whole
    // turn. The buffered path is lossy too (ureq's `Body::read_to_string`), so
    // the two agree on the malformed case rather than diverging on it.
    Ok((status, String::from_utf8_lossy(&all).into_owned()))
}

/// Build and send one POST, shared by the buffered and streaming reads so the
/// two cannot drift on headers, timeout, or status handling.
fn post(
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    // `Agent` is ureq's connection pool; building one per call is deliberate at
    // this stage (calls are rare and a shared agent would need a lifetime to
    // manage), and cheap enough that it is not the bottleneck.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request.send(body).map_err(|e| format!("{e}"))
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

/// Copy a directory tree, dereferencing symlinks.
///
/// Symlinks are followed so the copy is self-contained: a preset is copied
/// *out of* an install, and a copy holding links back into that install would
/// break the moment it is upgraded. The destination must not exist — a copy
/// never overwrites, and reporting that as `Exists` is what lets preset
/// authoring answer `agent-preset/invalid` instead of clobbering a preset.
///
/// On any failure the partial destination is removed: half a preset is
/// invisible to discovery at best and a mountable-but-incomplete one at worst.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if to.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    let result = copy_tree_inner(from, to);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(to);
    }
    result
}

fn copy_tree_inner(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree_inner(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to)?;
    }
    Ok(())
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
    drive_with(machine, input, &mut |_| {})
}

/// [`drive`], with a hook for the bytes a streaming effect produces.
///
/// A test that wants to observe incremental delivery passes a recorder; one that
/// only cares about the terminal outputs does not, which is why `drive` exists
/// as the no-op wrapper.
///
/// `on_effect` is called as each effect's answer is produced, with the machine's
/// outputs from the *chunk* deliveries already applied — but it is the *terminal*
/// outputs that are returned. A test asserting on incremental behaviour reads
/// what the hook saw, because terminal outputs are by construction the ones that
/// arrived after the effect, i.e. after the streaming was over.
#[cfg(test)]
pub fn drive_with(
    machine: &mut dyn PluginMachine<In = MachineIn, Out = MachineOut>,
    input: MachineIn,
    on_chunk: &mut dyn FnMut(&[MachineOut]),
) -> Vec<MachineOut> {
    let mut terminal: Vec<MachineOut> = Vec::new();
    let mut pending: std::collections::VecDeque<MachineIn> = std::collections::VecDeque::new();
    let mut todo: Vec<(EffectId, RealizeRequest)> = Vec::new();
    pending.push_back(input);
    let mut effects = 0;
    loop {
        if let Some(next) = pending.pop_front() {
            let outs = machine.handle(next);
            for out in outs {
                match out {
                    MachineOut::Realize { id, request } => todo.push((id, request)),
                    other => terminal.push(other),
                }
            }
            continue;
        }
        if todo.is_empty() || effects >= MAX_EFFECTS_PER_INPUT {
            break;
        }
        effects += 1;
        // A machine that awaits several effects issues them one at a time; at
        // most one is awaited per turn, and any others were fire-and-forget.
        let (id, request) = todo.remove(0);
        // Collected because the sink cannot borrow `pending`.
        let mut during: Vec<Vec<u8>> = Vec::new();
        let result =
            realize_with(request, &mut |bytes| during.push(bytes)).unwrap_or(EffectResult::Done);
        for bytes in during {
            let chunk_outs = machine.handle(MachineIn::EffectChunk { id, bytes });
            on_chunk(&chunk_outs);
            for out in chunk_outs {
                match out {
                    MachineOut::Realize { id, request } => todo.push((id, request)),
                    other => terminal.push(other),
                }
            }
        }
        pending.push_back(MachineIn::EffectResult { id, result });
    }
    terminal
}
