//! The `bash` tool's pure half: its request parsing and its result rendering.
//!
//! Upstream this is `dsh/packages/shell/tool-bash` — the Consumer of the
//! `ctx.shell` capability seam — with its model-facing text owned by
//! `render.ts`. The split here matches the fs family: [`super::tool`] holds the
//! shared vocabulary (`Outcome`, `Arguments`, the escalation hint) and this
//! module holds what is bash's own.
//!
//! ## Why this tool exists at all, and what it changes
//!
//! It is the host's **first tool that executes code**, and that is the whole
//! point: the sandbox runner seam ([`super::sandbox_runner`]) was complete and
//! kernel-verified but had no consumer, because `read`/`write`/`edit` execute
//! nothing and the fence over them is containment rather than a kernel boundary.
//! `bash` is what makes the runner's confinement load-bearing: a model-authored
//! command is the untrusted code the kernel sandbox exists to isolate.
//!
//! ## What is reduced here, and each is stated rather than hidden
//!
//! - **The completion notice is not delivered into the session.** Upstream's
//!   `tool-jobs` plugin listens on `jobs.onJobDone` and *injects a user-role
//!   message* when a job settles (`owner.inject`, or `followup` to wake an
//!   idle one, under a bounded wake budget). This host has no delivery channel
//!   for that: the assistant stream is owned by the turn's own pump, and a
//!   settle that reached the client without a `session/event` row would be a
//!   message the replay cannot see. A model that wants the completion asks
//!   with `job_output`, which is also the contract the tool's own prose names
//!   ("read its output with `job_output`"). The reduction is the absence of
//!   the notice, not its shape: no invented event type enters the log.
//! - **A background write is not a node of pump-object state.** Upstream's
//!   `jobs-local` is a *service*; this host's jobs table is a field on the
//!   agent machine, keyed by session, because the router offers no
//!   machine-to-machine call and the agent owns the catalog that mints jobs.
//! - **No stdin parameter, by upstream's design and not as a gap.** The
//!   model-facing bash tool does not expose stdin (`shell/src/types.ts`: "a
//!   model that needs stdin uses shell syntax"); the plumbing exists for
//!   in-process plugins and the `ProcessExec` effect carries it.
//! - **No per-call abort for a foreground run.** A turn cancel does not kill a
//!   *foreground* child (the effects are synchronous, and the dispatcher holds
//!   its lock across them). A background one is registered under the session's
//!   kill key, so a cancel *does* reach it — which is the upstream asymmetry
//!   verbatim: background work is what survives long enough to want one.
//!
//! None of these change the *shape* of a result: the envelope, the markers, and
//! the error classification are upstream's.

use serde_json::Value;

use super::sandbox::Mode;
use super::sandbox_runner::{Enforcement, Selection};
use super::tool::{Arguments, Outcome, display_path, escalation_hint};

/// The boot-resolved facts a confined command needs.
///
/// Pure data: the driver resolves it (probing runners and reading the
/// environment are I/O) and hands it to the agent machine at mount, so no
/// machine consults the process. It lives here rather than in `driver` so the
/// machines depend on the *shape* they consume, not on the I/O module that
/// produces it.
#[derive(Debug, Clone)]
pub struct SandboxContext {
    /// Which runner confines, if any. `Unavailable` is fail-closed.
    pub selection: Selection,
    /// The environment a model-authored command runs under: the fixed
    /// overrides, the resolved `PATH`, and the managed `DSH_*` facts.
    pub env: Vec<(String, String)>,
}

impl Default for SandboxContext {
    /// The fail-closed default: no runner, empty environment.
    ///
    /// Correct for a context that never runs a command — every existing test
    /// that drives a tool-calling turn but not bash. A host that can confine
    /// passes a resolved context instead; one that cannot refuses every command
    /// rather than running one unconfined.
    fn default() -> Self {
        Self {
            selection: Selection::Unavailable,
            env: Vec::new(),
        }
    }
}

/// The foreground timeout's default and cap, from `bash-local`'s `Config`:
/// `timeoutMs` (120 s) and `maxTimeoutMs` (600 s). The tool layer passes the
/// default and this host clamps to the cap, which is what `clampTimeout` does.
pub const BASH_DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const BASH_MAX_TIMEOUT_MS: u64 = 600_000;

/// The per-stream in-memory output cap (`maxOutputBytes`). The tail is kept and
/// the whole stream spills to disk past the bound (upstream's
/// `maxOutputBytes` + `maxSpillBytes` pair); the answered text reads the tail
/// and names the spill path.
pub const BASH_STDOUT_MAX_BYTES: usize = 64_000;
/// The spill file's cap (`bash-local`'s `maxSpillBytes`).
pub const BASH_MAX_SPILL_BYTES: usize = 64 * 1024 * 1024;

/// The environment overrides `bash-local` layers over the scrubbed parent
/// environment (`ENV_OVERRIDES`). A command's output is parsed by a model, not a
/// terminal, so color and paging are turned off and the locale-neutral `TERM` is
/// set — the same reason upstream fixes them rather than inheriting.
pub const ENV_OVERRIDES: &[(&str, &str)] = &[
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
];

/// The managed `DSH_SHELL` fact, set to `"1"` for a spawned command
/// (`shell-env/src/index.ts`'s `DSH_SHELL_KEY`). The other managed facts
/// (`DSH_HOME`, `DSH_SESSION_ID`) are assembled where their values live: the
/// home is a boot fact and the session id is machine state.
pub const DSH_SHELL: &str = "1";

/// The refusal a background request produces on a deployment with
/// `enableRunInBackground: false` — upstream's own sentence, retained so the
/// drifts a future reduction reintroduces have a wording to return to. Not
/// currently issued: the host composes a jobs registry, so a background call
/// is honored rather than refused.
#[allow(dead_code)]
pub const BACKGROUND_UNAVAILABLE: &str =
    "run_in_background is not available; long-running commands must finish within the timeout";

/// The acknowledgment a started background job answers with — upstream's
/// background output-union render (`index.ts`'s `render` over
/// `{kind: 'background'}`), verbatim: the model-facing text is the sentence,
/// and the job id is how the next three tools' parameters make sense.
pub fn started_background(job_id: &str) -> String {
    format!("started background job {job_id}")
}

/// What a background job's *start* outcome looks like once settled — the same
/// `processOutcome` vocabulary `tool-bash`'s background adapter uses: a kill
/// stays `killed` with its signal as detail, an exit reports its code, and a
/// shell that never ran reports no code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    /// The child is still running.
    Running,
    /// Settled on its own: an exit code (None on a signal-less death),
    /// reported rather than failed, exactly like the foreground path.
    Completed { exit_code: Option<i32> },
    /// Stopped by a kill — `job_kill`, or a session cancel that reached the
    /// child through its registration.
    Killed { signal: Option<i32> },
}

/// The `[status: ...]` marker upstream's `statusLine` renders.
///
/// Carried verbatim: a model that has learned the vocabulary sees the same
/// text for a still-running job and one that finished, and the distinction is
/// the basis for `job_output`'s "read what is new, then notice the settle"
/// rhythm.
pub fn status_line(status: &JobStatus, detail: Option<&str>) -> String {
    let status = match status {
        JobStatus::Running => "running",
        JobStatus::Completed { .. } => "completed",
        JobStatus::Killed { .. } => "killed",
    };
    match detail {
        Some(detail) if !detail.is_empty() => format!("[status: {status}, {detail}]"),
        _ => format!("[status: {status}]"),
    }
}

/// The detail a settled job's marker carries — `exit code: N` for a
/// completed run, `signal: S` for a killed one, both `background.ts`'s own
/// spellings.
pub fn settle_detail(status: &JobStatus) -> Option<String> {
    match status {
        JobStatus::Completed { exit_code } => {
            Some(format!("exit code: {}", exit_code.unwrap_or(0)))
        }
        JobStatus::Killed { signal } => {
            Some(match signal {
                Some(sig) => format!("signal: {}", signal_name(*sig)),
                None => "killed before exit".to_string(),
            })
        }
        JobStatus::Running => None,
    }
}

/// The full `job_output` body: any new output, then the status marker.
///
/// `output.render` in upstream's `tool-jobs`: `(no new output)` for an empty
/// delta, the text as-is otherwise, the marker after a forced final newline.
pub fn job_output_outcome(delta: &str, status: &JobStatus) -> Outcome {
    let detail = settle_detail(status);
    let body = if delta.is_empty() { "(no new output)" } else { delta };
    let separator = if body.ends_with('\n') { "" } else { "\n" };
    Outcome::text(format!("{body}{separator}{}", status_line(status, detail.as_deref())))
}

/// The `job_list` render — `(no background jobs)` for an empty table, one
/// `id [kind] status — label` line per job.
pub fn job_list_outcome(jobs: &[(String, JobStatus, String)]) -> Outcome {
    if jobs.is_empty() {
        return Outcome::text("(no background jobs)");
    }
    let lines: Vec<String> = jobs
        .iter()
        .map(|(id, status, label)| {
            let word = match status {
                JobStatus::Running => "running",
                JobStatus::Completed { .. } => "completed",
                JobStatus::Killed { .. } => "killed",
            };
            format!("{id} [bash] {word} — {label}")
        })
        .collect();
    Outcome::text(lines.join("\n"))
}

/// The `job_kill` render: `requested cancellation of job <id>` for live
/// work, `job <id> had already finished <status>` for a job the kill reached
/// too late.
pub fn job_kill_outcome(job_id: &str, already: Option<JobStatus>) -> Outcome {
    match already {
        Some(status) => Outcome::text(format!(
            "job {job_id} had already finished {}",
            status_line(&status, settle_detail(&status).as_deref())
        )),
        None => Outcome::text(format!("requested cancellation of job {job_id}")),
    }
}

/// The refusal a job control produces for an id the session does not own —
/// `jobs-local`'s own wording for the two cases, verbatim: unknown ids and
/// cross-session reads are one class because the fence is the point, not the
/// distinguishing.
pub fn unknown_job(job_id: &str, foreign: bool) -> Outcome {
    if foreign {
        Outcome::denied(format!("job {job_id} belongs to another session"))
    } else {
        Outcome::denied(format!("unknown job {job_id}"))
    }
}

/// A parsed, validated `bash` call, ready to be confined and run.
#[derive(Debug, Clone, PartialEq)]
pub struct BashRequest {
    /// The command line, as the model wrote it.
    pub command: String,
    /// The working directory, resolved. Always absolute by this point.
    pub workdir: String,
    /// The foreground timeout in milliseconds, clamped to the cap. Unused on a
    /// background call: upstream's schema says no timeout applies to one, and
    /// the detached effect carries none.
    pub timeout_ms: u64,
    /// Whether the call asked to run in the background. Parsed and honored:
    /// the schema now advertises the field, and the executor routes it to the
    /// detached effect rather than the blocking one.
    pub background: bool,
    /// The description the model wrote — display metadata for the UI on a
    /// foreground call, and the background job's *label* on a background one
    /// (upstream's `jobs.start({label})` is what `job_list` renders).
    pub description: String,
}

/// Parse and validate a `bash` call's arguments.
///
/// Mirrors `validateBashArgs` and `resolveWorkdir`. The value constraints the
/// JSON Schema cannot express (`command`/`description` non-empty, `timeoutMs`
/// positive) are checked here, as upstream checks them in `execute` for the same
/// reason: a schema declares types, and these are value rules.
///
/// `workdir` resolution: an explicit absolute path is used as given, a relative
/// one is joined against the session workspace root, and an absent one defaults
/// to that root — which is also the confinement root, so the two agree by
/// construction.
pub fn bash_request(args: &Arguments, root: &str) -> Result<BashRequest, String> {
    let Some(obj) = args.as_object() else {
        return Err("invalid arguments: expected an object".to_string());
    };
    let command = string_arg(obj, "command")
        .ok_or_else(|| "invalid command: expected a non-empty string".to_string())?;
    if command.trim().is_empty() {
        return Err("invalid command: expected a non-empty string".to_string());
    }
    // `description` is display metadata, but upstream requires it and a missing
    // one is a malformed call rather than a defaultable field.
    match string_arg(obj, "description") {
        Some(d) if !d.trim().is_empty() => {}
        _ => return Err("invalid description: expected a non-empty string".to_string()),
    }
    let description = string_arg(obj, "description").unwrap_or_default();
    let timeout_ms = match obj.get("timeoutMs") {
        None | Some(Value::Null) => BASH_DEFAULT_TIMEOUT_MS,
        Some(Value::Number(n)) => {
            let raw = n.as_u64().filter(|v| *v > 0).ok_or_else(|| {
                format!("invalid timeoutMs: expected a positive number, got {}", n)
            })?;
            raw.min(BASH_MAX_TIMEOUT_MS)
        }
        Some(other) => {
            return Err(format!(
                "invalid timeoutMs: expected a positive number, got {other}"
            ));
        }
    };
    let workdir = match obj.get("workdir").and_then(Value::as_str) {
        None => root.to_string(),
        Some(w) => display_path(root, w),
    };
    let background = obj
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(BashRequest {
        command,
        workdir,
        timeout_ms,
        background,
        description,
    })
}

/// A non-empty string argument, or `None`.
fn string_arg(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match obj.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// One finished command, as the renderer needs it.
///
/// Carries the process facts *and* the sandbox facts, because a result tells the
/// model two different things: what the command printed and whether the sandbox
/// allowed it to. Folding them into one string here is what keeps the executor
/// from re-deriving classification at render time.
#[derive(Debug, Clone, PartialEq)]
pub struct BashRun {
    pub exit_code: Option<i32>,
    /// The killing signal, when one terminated the child.
    pub signal: Option<i32>,
    pub timed_out: bool,
    /// The turn was cancelled and the child was killed for it. Upstream renders
    /// this to a `TOOL_ABORTED`-classed error ("The command was aborted"),
    /// distinct from the timeout so the model reads the stop as the user's
    /// rather than the clock's.
    pub aborted: bool,
    pub timeout_ms: u64,
    pub stdout: String,
    pub stderr: String,
    /// Whether the driver dropped output at the byte bound. Renders the
    /// truncation notice, which is how a model learns a stream was cut.
    pub truncated: bool,
    /// Where the untruncated stream spilled, when it did. `None` for a run that
    /// fit the bound, or one whose spill could not be written (the directory
    /// was not creatable) — which is what makes the suffix's `(unavailable)`
    /// branch still possible.
    pub spill_path: Option<String>,
    /// Whether the selected runner's dialect recognizes this stderr as a
    /// confinement refusal.
    pub denied: bool,
    pub mode: Mode,
    /// Carried for the schema's `sandbox.enforcement`; not rendered into text.
    pub enforcement: Enforcement,
}

/// Render a finished command into the model-facing result.
///
/// This is `renderResult`, and the text is a contract rather than a formatting
/// choice: the recorded corpus compares the marker lines byte for byte. The
/// order is load-bearing at both ends:
///
/// - **The exit marker is last** because `parseExitStatus` anchors there, so a
///   later line would be read as body text.
/// - **A signal death replaces the exit marker** rather than joining it: a
///   signal-killed child reports no exit code, and rendering the `None` as a
///   number would invent a status the OS never produced.
/// - **`isError` is false for everything here**, including a nonzero exit and a
///   sandbox denial. The corpus is explicit: a denial is a *result* the model
///   reads and reacts to, and upstream's doc says non-zero exits "are reported,
///   not errored". Only an infrastructure failure — the sandbox being
///   unavailable — is an error, and that is produced by the caller before a
///   command ever runs.
///
/// `escalation_available` mirrors upstream's `escalationModes.length > 0`: when
/// the composition advertises the escalation fields (this host does), a denial
/// carries the hint that tells the model the sanctioned retry.
pub fn bash_outcome(run: &BashRun, escalation_available: bool) -> Outcome {
    let mut body = run.stdout.clone();
    if run.truncated {
        // Upstream appends `[output truncated; full output: <spillPath>]` when
        // a spill wrote, and `(unavailable)` for the no-spill tail the shell
        // kept (`render.ts`). Some host must have written the path for a model
        // to recover the untruncated stream; a spill the disk refused reports
        // the same `--(unavailable)` it always has.
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("[output truncated; full output: ");
        body.push_str(run.spill_path.as_deref().unwrap_or("(unavailable)"));
        body.push(']');
    }
    if !run.stderr.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("[stderr]\n");
        body.push_str(&run.stderr);
    }
    if body.is_empty() {
        body = "(no output)".to_string();
    }

    let mut markers: Vec<String> = Vec::new();
    if run.denied {
        markers.push(denial_marker(run.mode));
        if escalation_available {
            markers.push(escalation_hint("command"));
        }
    }
    // A command may trap SIGTERM and exit 0 after the timeout fired; the
    // interruption is still a fact, so `timed_out` reports independently. A
    // turn cancel is the same fact under the user's name, and upstream's
    // wording for it is the AbortError message — the one marker that is *not*
    // a status.
    if run.aborted {
        markers.push("[aborted by the user]".to_string());
    }
    if run.timed_out {
        markers.push(format!("[timed out after {}ms]", run.timeout_ms));
    }
    if let Some(signal) = run.signal {
        markers.push(format!("[killed by signal: {}]", signal_name(signal)));
    } else if run.exit_code != Some(0) {
        // A `None` exit code here is a status the OS did not produce (a spawn
        // that never settled); naming it as a number would be a lie, so it is
        // reported as such rather than rendered.
        let code = run
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        markers.push(format!("[exit code: {code}]"));
    }
    if markers.is_empty() {
        return Outcome::text(body);
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&markers.join("\n"));
    Outcome::text(body)
}

/// The shared denial marker — one vocabulary for both enforcing families.
///
/// `sandboxDenialMarker`, verbatim. The filesystem family renders this too
/// ([`super::sandbox::Denial::message`]), so a model that has seen a denied
/// `write` recognizes a denied command.
fn denial_marker(mode: Mode) -> String {
    format!("[sandbox: file access denied under {} mode]", mode.as_str())
}

/// The name of a signal number, as the corpus writes it.
///
/// The marker is `[killed by signal: SIGTERM]`, a *name* rather than a number.
/// A closed table rather than a lookup against the host's `<signal.h>`: the
/// value must be identical everywhere the log is read, and a signal outside the
/// table is reported by number rather than dropped.
fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".to_string(),
        2 => "SIGINT".to_string(),
        3 => "SIGQUIT".to_string(),
        4 => "SIGILL".to_string(),
        6 => "SIGABRT".to_string(),
        8 => "SIGFPE".to_string(),
        9 => "SIGKILL".to_string(),
        11 => "SIGSEGV".to_string(),
        13 => "SIGPIPE".to_string(),
        14 => "SIGALRM".to_string(),
        15 => "SIGTERM".to_string(),
        other => other.to_string(),
    }
}

/// The result of a fail-closed escalation verdict, in bash's own words.
///
/// Upstream's `approveEscalation` throws `the user rejected escalating this
/// <subject> to "<mode>"` — subject `command` for bash, `operation` for the
/// filesystem family. Distinct from [`super::tool::approval_denial`]'s "rejected
/// tool" wording, which is what the fs family uses; the two differ because the
/// corpus records both and they are not interchangeable.
pub fn escalation_denial(requested: &str, outcome: &str) -> Outcome {
    match outcome {
        "rejected" => Outcome::denied(format!(
            "the user rejected escalating this command to \"{requested}\""
        )),
        "cancelled" => Outcome::denied(format!(
            "approval for escalating to \"{requested}\" was cancelled"
        )),
        _ => Outcome::denied(format!(
            "sandbox escalation to \"{requested}\" requires approval, but no approval channel is available"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Arguments {
        Arguments::parse(&v.to_string())
    }

    fn run(stdout: &str, stderr: &str, exit_code: Option<i32>) -> BashRun {
        BashRun {
            exit_code,
            signal: None,
            timed_out: false,
            aborted: false,
            timeout_ms: BASH_DEFAULT_TIMEOUT_MS,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            truncated: false,
            spill_path: None,
            denied: false,
            mode: Mode::WorkspaceWrite,
            enforcement: Enforcement::Full,
        }
    }

    /// The text of a rendered outcome's single block.
    fn text(o: &Outcome) -> String {
        o.content[0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn a_command_requires_a_non_empty_command_and_description() {
        assert!(bash_request(&args(json!({})), "/w").is_err());
        assert!(bash_request(&args(json!({ "command": "  ", "description": "d" })), "/w").is_err());
        assert!(bash_request(&args(json!({ "command": "ls" })), "/w").is_err());
        // A well-formed call parses.
        let r = bash_request(
            &args(json!({ "command": "ls", "description": "List files" })),
            "/w",
        )
        .expect("parses");
        assert_eq!(r.command, "ls");
        assert_eq!(r.workdir, "/w");
        assert_eq!(r.timeout_ms, BASH_DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn a_timeout_is_positive_and_clamped_to_the_cap() {
        let over = bash_request(
            &args(json!({ "command": "c", "description": "d", "timeoutMs": 9_000_000 })),
            "/w",
        )
        .expect("parses");
        assert_eq!(over.timeout_ms, BASH_MAX_TIMEOUT_MS);
        assert!(
            bash_request(
                &args(json!({ "command": "c", "description": "d", "timeoutMs": 0 })),
                "/w"
            )
            .is_err()
        );
        assert!(
            bash_request(
                &args(json!({ "command": "c", "description": "d", "timeoutMs": -1 })),
                "/w"
            )
            .is_err()
        );
    }

    #[test]
    fn a_relative_workdir_joins_the_workspace_root() {
        let rel = bash_request(
            &args(json!({ "command": "c", "description": "d", "workdir": "sub" })),
            "/w",
        )
        .expect("parses");
        assert_eq!(rel.workdir, "/w/sub");
        let abs = bash_request(
            &args(json!({ "command": "c", "description": "d", "workdir": "/etc" })),
            "/w",
        )
        .expect("parses");
        assert_eq!(abs.workdir, "/etc");
    }

    #[test]
    fn plain_output_is_the_body_and_carries_no_markers() {
        // `sdk/bash-tool` records exactly this: stdout, no marker, isError false.
        let o = bash_outcome(&run("dsh-sdk-proof-7391\n", "", Some(0)), true);
        assert_eq!(text(&o), "dsh-sdk-proof-7391\n");
        assert!(!o.is_error);
        assert!(o.meta.is_none(), "bash records no meta");
        assert!(o.error.is_none());
    }

    #[test]
    fn stderr_is_a_marked_section_and_exit_is_the_last_marker() {
        // `web/goal-multi-turn-actions`: body, then `[stderr]`, then the exit.
        let o = bash_outcome(
            &run("picked\n", "bash: shuf: command not found\n", Some(127)),
            true,
        );
        assert_eq!(
            text(&o),
            "picked\n[stderr]\nbash: shuf: command not found\n[exit code: 127]"
        );
        assert!(!o.is_error, "a nonzero exit is reported, not errored");
    }

    #[test]
    fn empty_output_renders_the_placeholder() {
        assert_eq!(
            text(&bash_outcome(&run("", "", Some(0)), true)),
            "(no output)"
        );
    }

    #[test]
    fn a_signal_death_replaces_the_exit_marker() {
        // `bash-startup-timeout`'s exact recorded text.
        let mut r = run("", "", None);
        r.timed_out = true;
        r.timeout_ms = 1;
        r.signal = Some(15);
        assert_eq!(
            text(&bash_outcome(&r, true)),
            "(no output)\n[timed out after 1ms]\n[killed by signal: SIGTERM]"
        );
    }

    #[test]
    fn a_denial_carries_the_marker_and_the_hint() {
        let mut r = run("", "", Some(1));
        r.denied = true;
        r.mode = Mode::ReadOnly;
        let o = bash_outcome(&r, true);
        let t = text(&o);
        assert!(
            t.contains("[sandbox: file access denied under read-only mode]"),
            "{t}"
        );
        assert!(t.contains("retry this exact command"), "{t}");
        // A denial is a result the model reacts to, not an infrastructure error.
        assert!(!o.is_error);
        assert!(o.error.is_none());
    }

    #[test]
    fn the_hint_is_omitted_when_escalation_is_not_advertised() {
        let mut r = run("", "", Some(1));
        r.denied = true;
        let t = text(&bash_outcome(&r, false));
        assert!(t.contains("file access denied"), "{t}");
        assert!(!t.contains("escalation available"), "{t}");
    }

    #[test]
    fn a_rejected_escalation_names_the_command_and_the_mode() {
        // `acp/escalation-rejected`'s exact text.
        let o = escalation_denial("danger-full-access", "rejected");
        assert_eq!(
            text(&o),
            "Error: the user rejected escalating this command to \"danger-full-access\""
        );
        assert!(o.is_error);
    }
}
