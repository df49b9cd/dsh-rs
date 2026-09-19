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

use vocoder_cordis::{
    EffectError, EffectId, EffectResult, MachineIn, MachineOut, PluginMachine, RealizeRequest,
};

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
        RealizeRequest::Raw(v) => {
            tracing::debug!("driver: unhandled Raw effect {v}");
            return None;
        }

        // Filesystem effects: answered back to the machine.
        RealizeRequest::ReadText { path } => match std::fs::read_to_string(&path) {
            Ok(text) => EffectResult::Text(text),
            Err(e) => EffectResult::Failed(io_err(&e)),
        },
        RealizeRequest::WriteText { path, contents } => {
            match write_atomically(std::path::Path::new(&path), &contents) {
                Ok(()) => EffectResult::Done,
                Err(e) => EffectResult::Failed(io_err(&e)),
            }
        }
        RealizeRequest::CreateDirAll { path } => match std::fs::create_dir_all(&path) {
            Ok(()) => EffectResult::Done,
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
    })
}

/// Write text via temp-file + rename, so a reader never observes a partial
/// file and a crash never leaves a truncated one. Parent directories are
/// created as needed.
///
/// Session generations depend on this: they are immutable once published, and
/// `vocoder-session`'s committed-artifact check treats any existing destination
/// as frozen.
pub fn write_atomically(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
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
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Drive one machine through the effect loop to quiescence.
///
/// Returns the machine's terminal outputs — crucially, outputs it produced
/// *after* the last effect answer, so a caller whose operation suspends on I/O
/// still sees its final reply. Used by machine unit tests; the live host does
/// the same thing through `AppState::pump`, which additionally routes stream
/// frames and dispatches events to other machines.
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
