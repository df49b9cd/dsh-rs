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
//! - **`run_in_background` is not offered.** Upstream registers it with
//!   `ctx.jobs`, a service this host does not compose; a background call would
//!   have no `job_output`/`job_kill` to collect it. The schema omits the field
//!   and the description takes upstream's own disabled-deployment sentence, so
//!   every string the model reads is still upstream's.
//! - **Output is bounded by a byte cap, not spilled to a file.** Upstream keeps
//!   a tail in memory and spills the whole stream to disk when it overflows,
//!   reporting the path; this host has no spill backend, so the truncation
//!   suffix is not emitted (there is no path to name) and the byte bound is
//!   applied by the driver.
//! - **No stdin and no per-call abort.** The spawn passes no stdin, and a turn
//!   cancel does not kill a running child — the effects are synchronous.
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

/// The per-stream in-memory output cap (`maxOutputBytes`).
pub const BASH_STDOUT_MAX_BYTES: usize = 64_000;

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

/// The refusal a background request produces.
///
/// Upstream's own sentence for a deployment with `enableRunInBackground: false`.
/// Copied so a model that learned the wording elsewhere recognizes it, and
/// because the field is genuinely absent rather than broken.
pub const BACKGROUND_UNAVAILABLE: &str =
    "run_in_background is not available; long-running commands must finish within the timeout";

/// A parsed, validated `bash` call, ready to be confined and run.
#[derive(Debug, Clone, PartialEq)]
pub struct BashRequest {
    /// The command line, as the model wrote it.
    pub command: String,
    /// The working directory, resolved. Always absolute by this point.
    pub workdir: String,
    /// The foreground timeout in milliseconds, clamped to the cap.
    pub timeout_ms: u64,
    /// Whether the call asked to run in the background. Always `false` today —
    /// the field is unadvertised — but parsed so an undeclared key is refused
    /// rather than silently ignored.
    pub background: bool,
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
    pub timeout_ms: u64,
    pub stdout: String,
    pub stderr: String,
    /// Whether the driver dropped output at the byte bound. Renders the
    /// truncation notice, which is how a model learns a stream was cut.
    pub truncated: bool,
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
        // Upstream appends `[output truncated; full output: <spillPath>]`; this
        // host has no spill backend, so the path slot reports `(unavailable)` —
        // upstream's own token for a spill that did not produce a file. Saying
        // nothing would let a model read a cut-off stream as a complete one.
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("[output truncated; full output: (unavailable)]");
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
    // interruption is still a fact, so `timed_out` reports independently.
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
            timeout_ms: BASH_DEFAULT_TIMEOUT_MS,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            truncated: false,
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
