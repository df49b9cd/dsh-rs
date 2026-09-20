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

/// A registry a cancel consults to reach a running child. Shared by the pump
/// (which registers each `ProcessExec` under its `kill_key` for the run's
/// length, while it keeps the only `Child` handle) and the cancel path (which
/// *removes* the entry to ask for the kill); the pump holds the dispatch lock
/// across the effect loop, so the registry is what lets the signal arrive
/// without waiting it out.
///
/// The value is just the pid: the runner owns the reap, so a kill is *the
/// runner noticing its entry gone* and killing its own handle — which is why
/// no second handle is ever taken and no wait is raced. Cancellation is
/// not-found == "already settled": the runner removes the entry when it exits,
/// so a cancel that lands after that finds nothing to stop.
/// Shared behind an `Arc`: a detached child's waiter thread outlives the call
/// that spawned it, so the registry cannot be a borrow — a `session/cancel`
/// and the waiter it wakes are both ownerless in the async host.
pub type ChildRegistry = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, u32>>>;

/// Remove `key`'s registration, asking the owning `run_process` to kill and
/// reap its child. Returns `true` when a live registration was there to drop;
/// the actual kill happens on the runner's next 5 ms poll.
pub fn kill_registered(registry: &ChildRegistry, key: &str) -> bool {
    registry.lock().unwrap().remove(key).is_some()
}

/// [`realize_with`]'s registry-less form, which the test path uses; the live
/// loop goes through [`realize_with_kills`] with the pump's registry.
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
#[cfg_attr(not(test), allow(dead_code))]
pub fn realize_with(
    request: RealizeRequest,
    sink: &mut dyn FnMut(Vec<u8>),
) -> Option<EffectResult> {
    realize_with_kills(request, sink, None, None)
}

/// [`realize_with`], with the child registry and detached table the process
/// effects register into.
///
/// `None`s are the pure form every other caller takes: a run with no registry
/// cannot be externally cancelled and a start with no table can never be read
/// back, so the two descend from the pump, which is the only caller that has
/// them.
pub fn realize_with_kills(
    request: RealizeRequest,
    sink: &mut dyn FnMut(Vec<u8>),
    children: Option<&ChildRegistry>,
    detached: Option<&std::sync::Arc<DetachedState>>,
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
        RealizeRequest::WriteText {
            path,
            contents,
            expect,
        } => {
            let path = std::path::Path::new(&path);
            // The guard is a re-check, not a trust: the version the machine
            // observed was taken by its own `Stat` effect, and between that and
            // this rename the world could change. Re-stat here and refuse a
            // mismatch *before* writing, mirroring the provider-side check in
            // upstream's `fs-local` (which holds a per-target lock; the write
            // itself is already temp+rename atomic).
            match &expect {
                vocoder_cordis::WriteExpect::Any => {}
                vocoder_cordis::WriteExpect::Absent => {
                    if path.try_exists().unwrap_or(false) {
                        return Some(EffectResult::Failed(EffectError::Exists));
                    }
                }
                vocoder_cordis::WriteExpect::Version(v) => {
                    let current = std::fs::metadata(path).ok().map(|m| version_token(&m));
                    match current {
                        Some(c) if &c == v => {}
                        Some(_) => {
                            return Some(EffectResult::Failed(EffectError::Other(format!(
                                "fs/stale-version: file changed since it was read\nrename {}",
                                path.display()
                            ))))
                        }
                        None => {
                            return Some(EffectResult::Failed(EffectError::Other(format!(
                                "fs/stale-version: file no longer exists\nrename {}",
                                path.display()
                            ))))
                        }
                    }
                }
            }
            match write_atomically(path, contents.as_bytes()) {
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
        // The argv arrives already wrapped in whatever confinement the machine
        // chose, so this is a plain spawn: the driver's whole contribution is
        // the process mechanics, and it never decides *whether* to confine.
        RealizeRequest::ProcessExec {
            argv,
            workdir,
            env,
            timeout_ms,
            stdout_max_bytes,
            spill_dir,
            stdin,
            kill_key,
        } => match run_process(
            &argv,
            workdir.as_deref(),
            &env,
            timeout_ms,
            stdout_max_bytes,
            spill_dir.as_deref(),
            stdin.as_deref(),
            kill_key.as_deref(),
            children,
        ) {
            Ok(done) => EffectResult::ProcessDone {
                exit_code: done.exit_code,
                signal: done.signal,
                stdout: done.stdout,
                stderr: done.stderr,
                truncated: done.truncated,
                timed_out: done.timed_out,
                aborted: done.aborted,
                spill_path: done.spill_path,
            },
            Err(e) => EffectResult::Failed(EffectError::Other(e)),
        },
        // Existence and executability, not execution. See the request's own doc
        // for why probing by running the program would defeat the probe.
        RealizeRequest::ProbeProgram { program } => EffectResult::Probe {
            found: program_is_executable(&program),
        },
        // Start, don't wait: the answer is the pid the later `ProcessRead` /
        // `ProcessKill` names. The argv is already confined (the same
        // structural fact `ProcessExec` carries), and the kill-key
        // registration is the same one a turn cancel reaches.
        RealizeRequest::ProcessStart {
            argv,
            workdir,
            env,
            stdout_max_bytes,
            spill_dir,
            kill_key,
        } => match detached {
            Some(table) => match start_process(
                &argv,
                workdir.as_deref(),
                &env,
                stdout_max_bytes,
                spill_dir.as_deref(),
                kill_key.as_deref(),
                children,
                table,
            ) {
                Ok(pid) => EffectResult::ProcessStarted { pid },
                Err(e) => EffectResult::Failed(EffectError::Other(e)),
            },
            // A start without a table would orphan the child, so refuse.
            None => EffectResult::Failed(EffectError::Other(
                "ProcessStart requires a detached-process table".to_string(),
            )),
        },
        // Drain the unread buffers and report the settle state; the machine
        // maps "no such pid" to the job-level refusal, so a read of a foreign
        // or invented id fails rather than hallucinating output.
        RealizeRequest::ProcessRead { pid } => match detached.and_then(|table| table.read(pid)) {
            Some((stdout_delta, stderr_delta, settled)) => {
                let (running, exit_code, signal, truncated, spill_path) = match settled {
                    Some((exit_code, signal, _aborted, truncated, spill_path)) => {
                        (false, exit_code, signal, truncated, spill_path)
                    }
                    None => (true, None, None, false, None),
                };
                EffectResult::ProcessChunk {
                    running,
                    stdout_delta,
                    stderr_delta,
                    exit_code,
                    signal,
                    truncated,
                    spill_path,
                }
            }
            None => EffectResult::Failed(EffectError::Other(format!("no such job: {pid}"))),
        },
        // Deliver the kill by removal: the waiter thread notices its
        // registration gone and terminates the handle it owns — the same
        // mechanism a `session/cancel` uses, so a `ProcessKill` and a cancel
        // are indistinguishable to the job. The read after is what reports the
        // settle; a kill is never its own confirmation.
        RealizeRequest::ProcessKill { pid } => match detached {
            Some(table) => {
                if table.kill(pid, children) {
                    EffectResult::Done
                } else {
                    EffectResult::Failed(EffectError::Other(format!("no such job: {pid}")))
                }
            }
            None => EffectResult::Failed(EffectError::Other(format!("no such job: {pid}"))),
        },
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

/// Whether a program can be executed: it exists and carries an execute bit.
///
/// Deliberately not `which`: the caller passes an absolute path (a sandbox
/// runner is located by the deployment, not searched for on `PATH`), and a
/// `PATH` search would introduce an environment dependency the machine cannot
/// see. A relative name is still accepted and resolved the way the OS would —
/// `Command` does its own `PATH` lookup at spawn time — but the answer here is
/// about the *path as given*, so a bare name reports "not found" rather than
/// pretending to have searched.
fn program_is_executable(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(program) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Probe the platform's sandbox chain and assemble the boot facts.
///
/// The probe is **functional**, not an existence check: a runner can be
/// installed and still unusable (an unprivileged-userns-disabled kernel is the
/// common case), so each rung is asked to confine a trivial command and the
/// verdict is whether that really ran. That is why this is a driver function
/// and not a machine one — it spawns processes.
///
/// A chain of one is selected without probing, which `select_runner` already
/// encodes: a sole candidate's own execution-time refusal is the fail-closed
/// end, and demanding a probe verdict for it would fail on a host where the
/// runner's *presence* is the only fact available (macOS, Windows).
pub fn probe_sandbox(platform: &str) -> crate::machines::tool_bash::SandboxContext {
    use crate::machines::sandbox_runner::{Runner, select_runner};
    let selection = select_runner(Runner::chain(platform), &mut |runner| {
        runner_usable(runner).then(|| runner.enforcement())
    });
    crate::machines::tool_bash::SandboxContext {
        selection,
        env: bash_env(),
    }
}

/// The environment a model-authored command runs under.
///
/// `ProcessExec` does `env_clear` then the given pairs, so nothing is inherited
/// — the server's environment holds provider credentials and must not reach a
/// model-authored command. What remains is the fixed overrides plus the two
/// facts a command genuinely needs: `PATH` (so `bash` can find its commands) and
/// the managed `DSH_*` facts upstream exposes. `DSH_HOME` is resolved from the
/// home the driver was given rather than read here, which is this module's usual
/// convention.
fn bash_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = crate::machines::tool_bash::ENV_OVERRIDES
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    // `PATH` is a process fact, resolved once here.
    if let Ok(path) = std::env::var("PATH") {
        env.push(("PATH".to_string(), path));
    }
    if let Ok(home) = std::env::var("HOME") {
        env.push(("DSH_HOME".to_string(), home));
    }
    env.push((
        "DSH_SHELL".to_string(),
        crate::machines::tool_bash::DSH_SHELL.to_string(),
    ));
    env
}

/// Whether one runner can confine a trivial command on this host.
///
/// The probe runs the runner's *real* profile around `true` under `read-only`,
/// so the answer is the kernel's rather than the binary's. `read-only` is the
/// right probe mode because its profile references no workspace: it grants only
/// `/dev/null`, so no session root is needed to ask the question.
fn runner_usable(runner: crate::machines::sandbox_runner::Runner) -> bool {
    use crate::machines::sandbox::Mode;
    use crate::machines::sandbox_runner::profile_args;
    let mut argv = vec![runner.program().to_string()];
    argv.extend(profile_args(runner, Mode::ReadOnly, "/"));
    argv.push("--".to_string());
    argv.push("true".to_string());
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
/// What one finished process reported.
///
/// Shared by the foreground path ([`run_process`], which builds it directly)
/// and the detached path ([`DetachedState`], which assembles it from what the
/// watcher thread drained) — a settled process carries the same facts however
/// long it took. Private to the driver; the cordis `EffectResult` is the
/// public record.
struct ProcessOutcome {
    exit_code: Option<i32>,
    /// The signal that killed the child, when one did. A signal death reports no
    /// exit code, so this is the only way a renderer can tell `[killed by
    /// signal: N]` from an exit status the OS never produced.
    signal: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
    timed_out: bool,
    /// A kill the cancel path asked for, distinct from the child's own
    /// timeout; a detached run reads it from the same latch the foreground
    /// wait polls, which is what makes a cancelled background job's settle
    /// identical to a cancelled foreground one's.
    aborted: bool,
    /// Where the untruncated output was written, if it overflowed and a spill
    /// directory was given.
    spill_path: Option<String>,
}

/// The signal that terminated a child, when one did.
///
/// `None` on a platform without POSIX signals, and for a normal exit. Both are
/// "no signal evidence", which is what the caller branches on.
#[cfg(unix)]
fn status_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn status_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Write the overflow's spill file, once, naming the path it landed at.
///
/// The whole stream is joined stdout-then-stderr with a `[stderr]` divider,
/// matching upstream's `spillAll`; the file is `O_EXCL 0o600` in a directory
/// the caller may or may not have created, so a host that cannot make the
/// directory reports the overflow with no path rather than pretending the
/// tail was whole.
fn write_spill(spill_dir: Option<&str>, stdout: &[u8], stderr: &[u8]) -> Option<String> {
    let dir = spill_dir?;
    let mut bytes = Vec::with_capacity(stdout.len() + stderr.len() + 16);
    bytes.extend_from_slice(stdout);
    if !stdout.is_empty() && !stderr.is_empty() {
        bytes.push(b'\n');
    }
    if !stderr.is_empty() {
        bytes.extend_from_slice(b"[stderr]\n");
        bytes.extend_from_slice(stderr);
    }
    let name = format!("{:016x}.log", rand_u64());
    let path = std::path::Path::new(dir).join(name);
    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }
    // 0o600 like upstream's spill: the stream may carry anything the
    // command read, and the directory beats a stranger's read.
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes))
        .ok()?;
    Some(path.display().to_string())
}

/// Spawn a child with the shared convention: the argv as given (already
/// confined by the machine — the arm is shared between `ProcessExec` and
/// `ProcessStart` precisely so the *whether* of confinement is decided once,
/// upstream of the driver), the caller's workdir, a scrubbed environment, and
/// piped streams.
fn spawn(
    argv: &[String],
    workdir: Option<&str>,
    env: &[(String, String)],
    with_stdin: bool,
) -> Result<std::process::Child, String> {
    let Some(program) = argv.first() else {
        return Err("ProcessExec requires a non-empty argv".to_string());
    };
    let mut command = std::process::Command::new(program);
    command.args(&argv[1..]);
    if let Some(dir) = workdir {
        command.current_dir(dir);
    }
    // `env_clear` then the given pairs: the server's environment holds provider
    // credentials, and a model-authored command must not inherit them.
    command.env_clear();
    for (k, v) in env {
        command.env(k, v);
    }
    if with_stdin {
        command.stdin(std::process::Stdio::piped());
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let child = command
        .spawn()
        .map_err(|e| format!("failed to spawn {program}: {e}"))?;
    Ok(child)
}

/// One reader thread's harvest from a pipe: the capped tail kept in memory and
/// the full-capped stream for a spill, as upstream's `OutputCollector` keeps
/// both (the tail is what the answer renders; the full stream is what the
/// spill protects against the tail's cut).
struct StreamLine {
    is_stdout: bool,
    tail: Vec<u8>,
    full: Vec<u8>,
    overflowed: bool,
}

/// Stream a pipe to its harvest, keeping the bounded tail and the spill source.
///
/// Every chunk is also handed to `on_chunk`, which is the detached path's
/// whole delta contract: a child's background buffer sees the same bytes in
/// the same order, so a `ProcessRead` drains exactly what arrived after its
/// last call — held in one tree so the foreground path's tail/overflow logic
/// and the background path's accumulation can never disagree about the bound.
fn read_stream(
    mut pipe: impl std::io::Read + Send,
    is_stdout: bool,
    bound: usize,
    on_chunk: &mut dyn FnMut(&[u8], bool),
    tx: std::sync::mpsc::Sender<StreamLine>,
) {
    let spill_cap = crate::machines::tool_bash::BASH_MAX_SPILL_BYTES;
    let mut tail: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    let mut full = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let bytes = &chunk[..n];
                if full.len() < spill_cap {
                    let take = (spill_cap - full.len()).min(n);
                    full.extend_from_slice(&bytes[..take]);
                }
                for &b in bytes {
                    if tail.len() == bound.max(1) {
                        tail.pop_front();
                    }
                    tail.push_back(b);
                }
                on_chunk(bytes, is_stdout);
            }
            Err(_) => break,
        }
    }
    let overflowed = full.len() > bound;
    let _ = tx.send(StreamLine {
        is_stdout,
        tail: tail.into_iter().collect(),
        full,
        overflowed,
    });
}

/// Join reader threads' harvests into per-stream records.
///
/// The pair is distinguished by the `is_stdout` flag rather than by arrival
/// order: a child without a pipe (a closed stream) sends no harvest, and an
/// ordering keyed on index would misattribute it.
struct Harvest {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    full_stdout: Vec<u8>,
    full_stderr: Vec<u8>,
    truncated: bool,
}

fn harvest(readers: usize, rx: &std::sync::mpsc::Receiver<StreamLine>) -> Harvest {
    let mut h = Harvest {
        stdout: Vec::new(),
        stderr: Vec::new(),
        full_stdout: Vec::new(),
        full_stderr: Vec::new(),
        truncated: false,
    };
    for _ in 0..readers {
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(line) => {
                h.truncated |= line.overflowed;
                if line.is_stdout {
                    h.stdout = line.tail;
                    h.full_stdout = line.full;
                } else {
                    h.stderr = line.tail;
                    h.full_stderr = line.full;
                }
            }
            // The reader thread died, or outlived a killed child's pipes. Its
            // output is already lost; reporting a spawn failure now would be
            // less accurate than reporting what came back.
            Err(_) => break,
        }
    }
    h
}

/// Run a process to completion, bounded in time and in output size.
///
/// Spawned from a thread rather than the async runtime because the effect
/// contract is synchronous (`realize_with` is called from `spawn_blocking`
/// already), so there is nothing to await on.
///
/// **Output is read on two threads and the child is killed on timeout.** A
/// single-threaded read-then-wait would deadlock the moment a child filled a
/// pipe buffer: the child blocks writing, the parent blocks waiting, and
/// neither moves. That failure is not hypothetical for a shell tool — `yes` is
/// one line of input away from it — so the reads happen concurrently with the
/// wait.
///
/// A timeout kills the child but still reports what it produced before the
/// kill, because "it timed out" and "here is what it printed first" are both
/// facts the model needs and the caller cannot recover the second one later.
fn run_process(
    argv: &[String],
    workdir: Option<&str>,
    env: &[(String, String)],
    timeout_ms: Option<u64>,
    stdout_max_bytes: Option<usize>,
    spill_dir: Option<&str>,
    stdin: Option<&str>,
    kill_key: Option<&str>,
    children: Option<&ChildRegistry>,
) -> Result<ProcessOutcome, String> {
    use std::io::Write;

    let program = argv.first().cloned().unwrap_or_default();
    let mut child = spawn(argv, workdir, env, true)?;

    // A turn-cancelable run is registered by its key, so the cancel path can
    // ask for the kill without waiting out the pump. The value is the pid; this
    // loop keeps the only `Child` and owns the reap, and it is this loop — not
    // the cancel — that calls `kill`, because the registration being *absent*
    // is the ask.
    let registered = kill_key.zip(children).map(|(key, registry)| {
        registry.lock().unwrap().insert(key.to_string(), child.id());
        (registry, key.to_string())
    });

    let bound = stdout_max_bytes.unwrap_or(usize::MAX);
    let (tx, rx) = std::sync::mpsc::channel::<StreamLine>();
    let mut readers = 0usize;
    for (is_stdout, pipe) in [
        (
            true,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ),
        (
            false,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ),
    ] {
        let Some(pipe) = pipe else { continue };
        let tx = tx.clone();
        readers += 1;
        std::thread::spawn(move || read_stream(pipe, is_stdout, bound, &mut |_, _| {}, tx));
    }
    drop(tx);

    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(text.as_bytes());
        // Dropping the handle closes the pipe, which is what lets a child
        // waiting on EOF proceed. Without it a `cat` would hang until timeout.
        drop(pipe);
    }

    let deadline =
        timeout_ms.map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    let mut timed_out = false;
    let mut aborted = false;
    let (exit_code, signal) = loop {
        // A cancel removes the entry; this loop notices on its next poll, kills
        // its own handle, and that is what marks the outcome an abort rather
        // than an ordinary nonzero exit. (The entry is removed on the way out
        // below, so a *settled* run reads present until it exits.)
        if let Some((registry, key)) = &registered
            && !timed_out
            && !aborted
            && !registry.lock().unwrap().contains_key(key)
        {
            aborted = true;
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), status_signal(&status)),
            Ok(None) => {}
            Err(e) => return Err(format!("failed to wait for {program}: {e}")),
        }
        if let Some(deadline) = deadline
            && std::time::Instant::now() >= deadline
        {
            let _ = child.kill();
            timed_out = true;
            match child.wait() {
                Ok(status) => break (status.code(), status_signal(&status)),
                Err(e) => return Err(format!("failed to reap {program}: {e}")),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    // Deregister: a later cancel for the same key must find nothing to kill.
    if let Some((registry, key)) = &registered {
        registry.lock().unwrap().remove(key);
    }

    let h = harvest(readers, &rx);

    // Write the spill only when something was actually dropped.
    let spill_path = if h.truncated {
        write_spill(spill_dir, &h.full_stdout, &h.full_stderr)
    } else {
        None
    };

    Ok(ProcessOutcome {
        exit_code,
        signal,
        stdout: String::from_utf8_lossy(&h.stdout).to_string(),
        stderr: String::from_utf8_lossy(&h.stderr).to_string(),
        truncated: h.truncated,
        timed_out,
        aborted,
        spill_path,
    })
}

// ---------------------------------------------------------------------------
// Detached processes: the `ProcessStart`/`ProcessRead`/`ProcessKill` triple
// ---------------------------------------------------------------------------

/// One settled detached child's terminal record, as a foreground answer's
/// `ProcessDone` carries it minus the timeout (a background run has none).
///
/// `Clone` because the table hands out copies: a `ProcessRead` of a settled
/// child stays answerable forever, and a moved record would leave a second
/// read unanswerable — upstream's own `jobs.read` is idempotent for exactly
/// this reason.
#[derive(Clone)]
struct Settled {
    exit_code: Option<i32>,
    signal: Option<i32>,
    aborted: bool,
    truncated: bool,
    /// Where the child's whole stream spilled, when it overflowed and a
    /// directory was given. Written once by the watcher, so every later read
    /// reports the same path — a reader that re-asked should see the file the
    /// first read promised, not a new best-effort write.
    spill_path: Option<String>,
}

/// What the driver keeps for one detached child, under one lock per pid.
///
/// One lock *per pid* (an entry in the map) rather than one per table: a slow
/// drain of a log-heavy child must not pin the table a `ProcessKill` needs to
/// reach. The unread-output two strings and the settle record live together
/// because a drain and a settle race *through the same pid* — split maps
/// would let a read observe output without seeing the settle that produced
/// its last chunk.
#[derive(Default)]
struct Background {
    /// stdout and stderr the reader threads have delivered and no
    /// `ProcessRead` has claimed yet. Drained by the read, which is upstream's
    /// `jobs.read` contract: the tool reports deltas, not a growing whole.
    stdout: String,
    stderr: String,
    /// The settle record, once the watcher thread has written it.
    settled: Option<Settled>,
    /// The kill key the child registered under, when the starter named one.
    /// Kept on the record because a `ProcessKill` names a *pid* and the
    /// registry a *key* — the table is the map between them.
    kill_key: Option<String>,
}

/// The detached-process table: pid → its unread buffers and settle state.
///
/// Held on `AppState` beside the [`ChildRegistry`] because the two share the
/// one invariant — a pid a detached reader is draining into is exactly a pid
/// this table owns, so a `ProcessKill` removing the entry *is* the reap a
/// foreground wait performs. The bucket is an `Arc` for the reverse reason:
/// the reader threads a `ProcessStart` spawns must outlive the effect call
/// that started them, which an `AppState` borrow cannot do.
pub struct DetachedState {
    map: std::sync::Mutex<std::collections::HashMap<u32, Background>>,
}

impl DetachedState {
    /// An empty table, for the AppState field's initializer.
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Deliver one chunk a reader just produced.
    ///
    /// Appending is all it does: the cap was applied at the read, per stream,
    /// and the table's whole job is to hold what the tool has not asked for
    /// yet.
    fn push(&self, pid: u32, text: &str, is_stdout: bool) {
        let mut map = self.map.lock().unwrap();
        let entry = map.entry(pid).or_default();
        if is_stdout {
            entry.stdout.push_str(text);
        } else {
            entry.stderr.push_str(text);
        }
    }

    /// Drain the pid's unread output, and report its settle state if any.
    ///
    /// `None` answers "no such pid" — the machine maps it to the job-level
    /// refusal, which is where the `job_output`-on-an-unknown-id error lives.
    fn read(
        &self,
        pid: u32,
    ) -> Option<(
        String,
        String,
        Option<(Option<i32>, Option<i32>, bool, bool, Option<String>)>,
    )> {
        let mut map = self.map.lock().unwrap();
        let entry = map.get_mut(&pid)?;
        let stdout = std::mem::take(&mut entry.stdout);
        let stderr = std::mem::take(&mut entry.stderr);
        let settled = entry.settled.as_ref().map(|s| {
            (
                s.exit_code,
                s.signal,
                s.aborted,
                s.truncated,
                s.spill_path.clone(),
            )
        });
        Some((stdout, stderr, settled))
    }

    /// Seal the record: the child has settled, and the watcher has named its
    /// spill.
    fn settle(&self, pid: u32, settled: Settled) {
        let mut map = self.map.lock().unwrap();
        let entry = map.entry(pid).or_default();
        // Keep any unread bytes: a job killed mid-burst still owes the final
        // `job_output` what the child wrote before the kill.
        entry.settled = Some(settled);
    }

    /// Whether a pid was ever recorded here — the ownership check a
    /// `ProcessKill` answers by.
    fn contains(&self, pid: u32) -> bool {
        self.map.lock().unwrap().contains_key(&pid)
    }

    /// Ask for the pid's kill: remove its registration under the kill key, so
    /// the waiter thread's poll (the same code a turn cancel rides) terminates
    /// the child. `false` is an unknown pid — a record *settled* is still a
    /// record, matching upstream's `already-finished` split, which the tool
    /// reads from the settle it then collects.
    fn kill(&self, pid: u32, children: Option<&ChildRegistry>) -> bool {
        let key = self.map.lock().unwrap().get(&pid).and_then(|b| b.kill_key.clone());
        if key.is_none() && !self.contains(pid) {
            return false;
        }
        match (key, children) {
            (Some(key), Some(registry)) => registry.lock().unwrap().remove(&key).is_some()
                || self.contains(pid),
            _ => self.contains(pid),
        }
    }

    /// Record the kill key at start, so `kill` need not be told it again.
    fn name_key(&self, pid: u32, key: Option<String>) {
        self.map.lock().unwrap().entry(pid).or_default().kill_key = key;
    }
}

/// Start a detached process: spawn, wire the readers into the shared table,
/// park a waiter on its own thread, and answer with the pid.
///
/// The spawn+reader setup is `run_process`'s own — it has to be, because the
/// confinement contract (the argv *already wrapped* fact of
/// [`RealizeRequest::ProcessStart`]) is only honored by reusing the path the
/// machine's confining decision reaches. What differs is the harvest: the
/// foreground path joins the pipes for one settle; this one hands each chunk
/// to the table and lets a waiter thread close the record, so the pump is
/// free for the next call — which is the whole reason `ProcessStart` exists
/// rather than a `ProcessExec` whose answer arrives in a later turn.
fn start_process(
    argv: &[String],
    workdir: Option<&str>,
    env: &[(String, String)],
    stdout_max_bytes: Option<usize>,
    spill_dir: Option<&str>,
    kill_key: Option<&str>,
    children: Option<&ChildRegistry>,
    detached: &std::sync::Arc<DetachedState>,
) -> Result<u32, String> {
    let mut child = spawn(argv, workdir, env, false)?;
    let pid = child.id();
    let bound = stdout_max_bytes.unwrap_or(usize::MAX);

    // Register the record *before* anything can read it: a `ProcessRead` of
    // the just-answered pid must find the table holding it, and recording the
    // kill key on the entry is what lets a later `ProcessKill` turn a pid back
    // into the registration it must remove.
    detached.name_key(pid, kill_key.map(|k| k.to_string()));

    // The cancel registration works exactly as for a foreground run: the
    // entry's *absence* is the ask, and the waiter thread (below) polls for it.
    // Reusing the same key vocabulary is what makes a `session/cancel` reach a
    // background job without a second registry — and the entry's own
    // deregistration happens inside the waiter, so a cancel after the settle
    // finds nothing to kill.
    let registered = kill_key.zip(children).map(|(key, registry)| {
        registry.lock().unwrap().insert(key.to_string(), pid);
        (std::sync::Arc::clone(registry), key.to_string())
    });

    let (tx, rx) = std::sync::mpsc::channel::<StreamLine>();
    let mut readers = 0usize;
    for (is_stdout, pipe) in [
        (
            true,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ),
        (
            false,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ),
    ] {
        let Some(pipe) = pipe else { continue };
        let tx = tx.clone();
        readers += 1;
        let table = std::sync::Arc::clone(detached);
        std::thread::spawn(move || {
            read_stream(pipe, is_stdout, bound, &mut |bytes, _| {
                table.push(pid, &String::from_utf8_lossy(bytes), is_stdout);
            }, tx);
        });
    }
    drop(tx);

    // The waiter: owns the `Child` handle (nobody may reap it asking), polls
    // for the settle or the kill ask, and writes the terminal record into the
    // table once. A plain thread rather than the pump: a detached child lasts
    // longer than the call that started it, which is the entire point.
    let spill = spill_dir.map(|d| d.to_string());
    let table = std::sync::Arc::clone(detached);
    std::thread::spawn(move || {
        let mut aborted = false;
        let (exit_code, signal) = loop {
            if let Some((registry, key)) = &registered
                && !aborted
                && !registry.lock().unwrap().contains_key(key)
            {
                aborted = true;
                let _ = child.kill();
            }
            match child.try_wait() {
                Ok(Some(status)) => break (status.code(), status_signal(&status)),
                Ok(None) => {}
                // A wait error is a settle the OS refused to report; record it
                // with no code rather than leave the record open indefinitely.
                Err(_) => break (None, None),
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        if let Some((registry, key)) = &registered {
            registry.lock().unwrap().remove(key);
        }
        let h = harvest(readers, &rx);
        let spill_path = if h.truncated {
            write_spill(spill.as_deref(), &h.full_stdout, &h.full_stderr)
        } else {
            None
        };
        table.settle(
            pid,
            Settled {
                exit_code,
                signal,
                aborted,
                truncated: h.truncated,
                spill_path,
            },
        );
        // The readers pushed every chunk as it arrived, so the table's
        // *unread* half already holds whatever the tool has not read — including
        // the tail truncated out of the in-memory bound, which this host's
        // cap drop is faithful about: the spill path names where the rest is.
        let _ = (h.stdout, h.stderr);
    });
    Ok(pid)
}

/// A best-effort random u64 for a spill file's name.
///
/// Upstream uses a random name inside a private `0700` directory; the directory
/// is the access control and the name only needs to not collide, so a
/// `/dev/urandom` draw with a pid/time fallback is enough.
#[cfg(unix)]
fn rand_u64() -> u64 {
    use std::io::Read;
    let mut b = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom")
        && f.read_exact(&mut b).is_ok()
    {
        return u64::from_le_bytes(b);
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    seed ^ ((std::process::id() as u64) << 32)
}

/// Render collected bytes as text, honoring the byte bound.
///
/// Lossy rather than fallible: a command's output is not required to be UTF-8
/// (a compiler diagnostic on a path with invalid bytes is enough), and refusing
/// the whole result over one bad byte would be worse than a replacement
/// character. The caller that needs exact bytes is not a shell tool.
#[allow(dead_code)]
fn bound_text(mut bytes: Vec<u8>, bound: usize) -> String {
    if bytes.len() > bound {
        bytes.truncate(bound);
    }
    String::from_utf8_lossy(&bytes).to_string()
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

/// The sink's flush anchor, not a correctness boundary: a harness's `data` can
/// split mid-line, which is exactly why this reader uses `read_line` — the
/// decoder already knows what a frame looks like and does not need raw slices
/// on its input side.

/// POST a JSON body and read the response incrementally, handing each line to
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
    use std::io::BufRead;
    let response = post(url, headers, body)?;
    let status = response.status().as_u16();
    let mut reader = std::io::BufReader::new(response.into_body().into_reader());
    let mut all = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let line = std::mem::take(&mut line);
                all.push_str(&line);
                sink(line.into_bytes());
            }
            Err(e) => return Err(format!("read response body: {e}")),
        }
    }
    Ok((status, all))
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
    // The test host's own halves of the process plumbing: a test driving a
    // background call through the machine hits the same effects the live pump
    // does, which is what keeps the whole-host test honest about the seam.
    let registry: ChildRegistry =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let detached = DetachedState::new();
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
            realize_with_kills(
                request,
                &mut |bytes| during.push(bytes),
                Some(&registry),
                Some(&detached),
            )
            .unwrap_or(EffectResult::Done);
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

#[cfg(test)]
mod process_tests {
    use super::*;

    /// A registered run is killed by removing its key; the answer reports the
    /// aborted kill, and the child's output up to the kill is still carried.
    #[cfg(unix)]
    #[test]
    fn a_cancelled_run_aborts_instead_of_timing_out() {
        let registry: ChildRegistry = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let req = vocoder_cordis::RealizeRequest::ProcessExec {
            argv: vec!["bash".into(), "-c".into(), "echo hi; sleep 5".into()],
            workdir: None,
            env: vec![],
            timeout_ms: Some(30_000),
            stdout_max_bytes: Some(1_000),
            spill_dir: None,
            stdin: None,
            kill_key: Some("s1".into()),
        };
        let reg = &registry;
        let run = std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel();
            s.spawn(move || {
                let r = realize_with_kills(
                    req,
                    &mut |_| {},
                    Some(reg),
                    Some(&DetachedState::new()),
                );
                let _ = tx.send(r);
            });
            // Wait for the registration to land, then cancel.
            for _ in 0..200 {
                if registry.lock().unwrap().contains_key("s1") {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(kill_registered(&registry, "s1"));
            rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap()
        });
        let Some(EffectResult::ProcessDone {
            aborted,
            timed_out,
            stdout,
            ..
        }) = run
        else {
            panic!("expected ProcessDone");
        };
        assert!(aborted, "a killed run reports aborted");
        assert!(!timed_out, "an abort is not the deadline firing");
        assert_eq!(stdout, "hi\n");
    }

    /// An overflow keeps the **tail** in the answer and spills the whole stream
    /// into the given directory — the upstream `OutputCollector` contract, down
    /// to the file being where `run.spill_path` says. A run that *fits* spills
    /// nothing.
    #[cfg(unix)]
    #[test]
    fn a_process_past_the_bound_spills_the_full_stream() {
        let dir = tempfile::tempdir().unwrap();
        let big = vocoder_cordis::RealizeRequest::ProcessExec {
            argv: vec![
                "bash".into(),
                "-c".into(),
                "head -c 200000 /dev/zero | tr '\\0' 'x'".into(),
            ],
            workdir: None,
            env: vec![],
            timeout_ms: Some(10_000),
            stdout_max_bytes: Some(1_000),
            spill_dir: Some(dir.path().display().to_string()),
            stdin: None,
            kill_key: None,
        };
        let EffectResult::ProcessDone {
            stdout,
            truncated,
            spill_path,
            ..
        } = realize_with(big, &mut |_| {}).unwrap()
        else {
            panic!("expected ProcessDone");
        };
        assert!(truncated);
        // The answer carries the tail, never the head: the last bytes a model
        // sees are the freshest, which is what a truncating terminal shows.
        assert_eq!(stdout.len(), 1_000);
        assert!(stdout.chars().all(|c| c == 'x'));
        let spill = std::fs::read_to_string(spill_path.expect("a spill was written")).unwrap();
        assert_eq!(spill.len(), 200_000);
        assert!(spill.chars().all(|c| c == 'x'));

        // A run under the bound does not spill.
        let fits = vocoder_cordis::RealizeRequest::ProcessExec {
            argv: vec!["printf".into(), "ok".into()],
            workdir: None,
            env: vec![],
            timeout_ms: Some(10_000),
            stdout_max_bytes: Some(1_000),
            spill_dir: Some(dir.path().display().to_string()),
            stdin: None,
            kill_key: None,
        };
        let EffectResult::ProcessDone {
            truncated,
            spill_path,
            ..
        } = realize_with(fits, &mut |_| {}).unwrap()
        else {
            panic!("expected ProcessDone");
        };
        assert!(!truncated);
        assert!(spill_path.is_none());
    }

    /// A detached start answers immediately with a pid; reads drain the delta
    /// as it arrives and report the settle once the child finishes, and a
    /// session cancel reaches it through the same registration a foreground
    /// run uses.
    #[cfg(unix)]
    #[test]
    fn a_detached_run_is_started_read_and_cancelled() {
        let registry: ChildRegistry =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let detached = DetachedState::new();

        // Start: the child closes on its own, having had time to print.
        let start = realize_with_kills(
            RealizeRequest::ProcessStart {
                argv: vec!["bash".into(), "-c".into(), "echo hi; sleep 30".into()],
                workdir: None,
                env: vec![],
                stdout_max_bytes: Some(1_000),
                spill_dir: None,
                kill_key: Some("s1".into()),
            },
            &mut |_| {},
            Some(&registry),
            Some(&detached),
        );
        let Some(EffectResult::ProcessStarted { pid }) = start else {
            panic!("expected ProcessStarted, got {start:?}");
        };
        // The registration is live: the cancel path can already see it.
        assert!(registry.lock().unwrap().contains_key("s1"));

        // A first read, polling until the shell printed — the drain is a
        // delta by construction, so a later read of the same output is the
        // test's own witness to the delta contract.
        let mut got = String::new();
        let mut running = true;
        for _ in 0..400 {
            let read = realize_with_kills(
                RealizeRequest::ProcessRead { pid },
                &mut |_| {},
                Some(&registry),
                Some(&detached),
            );
            let Some(EffectResult::ProcessChunk {
                running: r,
                stdout_delta,
                ..
            }) = read
            else {
                panic!("expected ProcessChunk, got {read:?}");
            };
            got.push_str(&stdout_delta);
            running = r;
            if got.contains("hi") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, "hi\n");
        assert!(running, "the sleep is still going");

        // The cancel kills it — same mechanism, same answer: the next read
        // reports the settle with `aborted…: the kill's own aftermath.
        assert!(kill_registered(&registry, "s1"));
        let mut settled = false;
        for _ in 0..400 {
            let read = realize_with_kills(
                RealizeRequest::ProcessRead { pid },
                &mut |_| {},
                Some(&registry),
                Some(&detached),
            );
            let Some(EffectResult::ProcessChunk {
                running, signal, ..
            }) = read
            else {
                panic!("expected ProcessChunk");
            };
            if !running {
                assert_eq!(signal, Some(9).or(Some(15)), "a killed job ends by signal");
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(settled, "the child settled after the cancel");

        // A kill of an unknown pid is the refusal, not a signal into the void.
        for unknown in [1u32, u32::MAX] {
            let kill = realize_with_kills(
                RealizeRequest::ProcessKill { pid: unknown },
                &mut |_| {},
                Some(&registry),
                Some(&detached),
            );
            assert!(
                matches!(kill, Some(EffectResult::Failed(_))),
                "pid {unknown}: {kill:?}"
            );
        }
    }

    /// A detached `ProcessKill` asks through the same registry the cancel
    /// uses, so a job's own kill is indistinguishable from a session cancel.
    #[cfg(unix)]
    #[test]
    fn a_detached_kill_reaches_the_child() {
        let registry: ChildRegistry =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let detached = DetachedState::new();
        let handle = std::sync::Arc::clone(&detached);
        let start = realize_with_kills(
            RealizeRequest::ProcessStart {
                argv: vec!["sleep".into(), "30".into()],
                workdir: None,
                env: vec![],
                stdout_max_bytes: None,
                spill_dir: None,
                kill_key: Some("s2".into()),
            },
            &mut |_| {},
            Some(&registry),
            Some(&handle),
        );
        let Some(EffectResult::ProcessStarted { pid }) = start else {
            panic!("expected ProcessStarted");
        };
        assert!(registry.lock().unwrap().contains_key("s2"));

        let kill = realize_with_kills(
            RealizeRequest::ProcessKill { pid },
            &mut |_| {},
            Some(&registry),
            Some(&handle),
        );
        assert!(matches!(kill, Some(EffectResult::Done)));

        // The registration vanishes as soon as the driver can process it —
        // the test witnesses the poll within its own budget.
        let mut settled = false;
        for _ in 0..400 {
            let read = realize_with_kills(
                RealizeRequest::ProcessRead { pid },
                &mut |_| {},
                Some(&registry),
                Some(&handle),
            );
            let Some(EffectResult::ProcessChunk { running, .. }) = read else {
                panic!("expected ProcessChunk");
            };
            if !running {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(settled, "the killed child settled");
        assert!(!registry.lock().unwrap().contains_key("s2"));
    }
}
