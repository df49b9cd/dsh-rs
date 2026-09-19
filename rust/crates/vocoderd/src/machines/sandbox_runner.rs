//! The kernel sandbox: the runner chain, the confinement profiles, and the
//! classification of what a confined process reported.
//!
//! `sandbox.rs` is the *containment fence* — a policy check in trusted code over
//! a model-controlled path, which is the right answer when the only untrusted
//! thing is a filename. This module is the other half: confinement of untrusted
//! **code**, where the untrusted thing is a whole process and a policy check
//! cannot help, because the process can make syscalls the checker never sees. The
//! boundary has to be the kernel's.
//!
//! ## The shape, and why it is split this way
//!
//! Upstream's `LocalSandboxProvider.confine` is a **pure function**: given an
//! argv and a policy it returns the wrapped argv plus the metadata needed to
//! interpret what the run produced — an enforcement verdict, the denial
//! signatures that backend's kernel speaks, and structured runner-failure rules
//! (`dsh/packages/sandbox/sandbox-local/src/index.ts`). That is already a
//! machine's signature, and it is the seam this module reproduces.
//!
//! The split matters because it puts everything interesting on this side of the
//! process boundary. Which runner is selected, what its profile arguments are,
//! whether a nonzero exit means "denied" or "the runner never started" — all of
//! that is decided here, purely, and tested without spawning anything. The
//! driver's half is `Command::new(argv[0]).args(argv[1..])`.
//!
//! ## Fail closed, and the one thing that is not negotiable
//!
//! The service contract in `api-catalog.ts` states it directly:
//!
//! > `confine` must return enforcing argv or fail closed at wrap or
//! > runner-execution time; silent unconfined passthrough is forbidden.
//!
//! So there is no arm of this module that returns the original argv for a
//! confined mode. When no runner is usable the call **fails**, with the message
//! the corpus records verbatim:
//!
//! > `sandbox mode "read-only" is requested but no sandbox backend is usable on
//! > this host; refusing to run the command unconfined. …`
//!
//! The alternative — running unconfined and reporting success — is the failure
//! mode that makes a sandbox worse than none, because the caller believes it was
//! confined.

// The module is complete and tested but **not yet wired**: the executor that
// drives it is the next step, so in a non-test build every item here reads as
// dead code. The allow is scoped to the module rather than sprinkled per item so
// that removing it later is one deletion, and so the reason is stated once.
// `sandbox.rs` and `agent_loop.rs` carry the same allow for the same reason.
#![allow(dead_code)]

use serde_json::{Value, json};

use super::sandbox::{Mode, writable_roots_for};

/// A runner that can confine a process on this platform.
///
/// The set is closed and platform-scoped, mirroring upstream's
/// `PLATFORM_CHAINS`: Linux prefers `bwrap` because its mount profile is closest
/// to the mode vocabulary, and falls back to the Landlock launcher; macOS has
/// exactly one candidate (Seatbelt); Windows has one (the ACL restricted token).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    Bwrap,
    Landlock,
    Seatbelt,
    WindowsAcl,
}

impl Runner {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bwrap => "bwrap",
            Self::Landlock => "landlock",
            Self::Seatbelt => "seatbelt",
            Self::WindowsAcl => "windows-acl",
        }
    }

    /// The runner chain for a platform, in preference order.
    ///
    /// Selection is **by platform first, probes second**: a platform's chain is
    /// probed only when it has more than one candidate. Probing arbitrates; it
    /// does not re-validate a choice that has no alternative. An unlisted
    /// platform has no chain and therefore fails closed.
    pub fn chain(platform: &str) -> &'static [Runner] {
        match platform {
            "linux" => &[Self::Bwrap, Self::Landlock],
            "macos" => &[Self::Seatbelt],
            "windows" => &[Self::WindowsAcl],
            _ => &[],
        }
    }

    /// How completely this runner governs the promised file effects.
    ///
    /// `partial` is a real claim, not a hedge: the Windows ACL rung cannot govern
    /// every ACL-addressable path (its restricting list must retain `Everyone`
    /// for process initialization, and NTFS hard links alias one file object
    /// across paths), so it must not advertise the absolute promise. The other
    /// three govern every promised effect by construction.
    pub fn enforcement(self) -> Enforcement {
        match self {
            Self::WindowsAcl => Enforcement::Partial,
            _ => Enforcement::Full,
        }
    }

    /// The denial dialect this runner's kernel speaks.
    ///
    /// The case-insensitive stderr substrings a denied file effect produces under
    /// it. These are *kernel* messages, not runner messages, which is why they
    /// are per-runner rather than shared: a Landlock denial and a Seatbelt denial
    /// say different things because different mechanisms refused.
    pub fn denial_signatures(self) -> &'static [&'static str] {
        match self {
            Self::Bwrap => &["read-only file system"],
            Self::Landlock => &["permission denied"],
            Self::Seatbelt => &["operation not permitted"],
            Self::WindowsAcl => &[
                "access is denied",
                "access to the path",
                "permission denied",
            ],
        }
    }

    /// Runner-owned fatal diagnostics, as structured rules.
    ///
    /// See [`failure_rules`].
    pub fn failure_rules(self) -> Vec<RunnerFailureRule> {
        match self {
            Self::Bwrap => vec![RunnerFailureRule::any_exit(&["bwrap: "])],
            Self::Landlock => vec![RunnerFailureRule {
                allowed_exit_codes: Some(vec![LANDLOCK_FAILURE_EXIT]),
                fatal_signatures: vec!["landlock-run: ".to_string()],
                // The launcher self-reports partial enforcement on stderr at
                // *every* confined run, so this line is expected output rather
                // than evidence of a failure. Excluding it by exact full-line
                // equality is what keeps a successful run from being classified
                // as a runner that never started.
                informational_lines: vec![PARTIAL_ENFORCEMENT_NOTICE.to_string()],
            }],
            Self::Seatbelt => vec![RunnerFailureRule::any_exit(&["sandbox-exec: "])],
            Self::WindowsAcl => vec![RunnerFailureRule {
                allowed_exit_codes: Some(vec![WINDOWS_ACL_FAILURE_EXIT]),
                fatal_signatures: vec!["windows-acl-run: ".to_string()],
                informational_lines: vec![],
            }],
        }
    }

    /// The program to probe and to invoke, for this platform.
    ///
    /// The Landlock rung is not a `PATH` program: it is a launcher binary shipped
    /// with the host addon, so its path is a deployment fact. It is named here as
    /// the bare launcher name and the deployment is expected to place it on
    /// `PATH`; a host that ships it elsewhere overrides this.
    pub fn program(self) -> &'static str {
        match self {
            Self::Bwrap => "bwrap",
            Self::Landlock => "landlock-run",
            Self::Seatbelt => "sandbox-exec",
            Self::WindowsAcl => "windows-acl-run",
        }
    }
}

/// Enforcement completeness for a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// Every promised file effect is governed.
    Full,
    /// Some promised file effect is not (see [`Runner::enforcement`]).
    Partial,
}

impl Enforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Partial => "partial",
        }
    }
}

/// The Landlock launcher's documented failure exit.
pub const LANDLOCK_FAILURE_EXIT: i32 = 125;
/// The Windows ACL runner's documented failure exit.
pub const WINDOWS_ACL_FAILURE_EXIT: i32 = 127;
/// The Landlock launcher's informational line for an older-ABI partial run.
pub const PARTIAL_ENFORCEMENT_NOTICE: &str =
    "landlock-run: partial enforcement (older Landlock ABI)";
/// The prefix the launcher puts on its own failure messages.
pub const LANDLOCK_BIN: &str = "landlock-run";

/// Evidence that identifies a sandbox runner failing *before it executed the
/// wrapped command*.
///
/// The distinction this exists to draw: a confined command that exits 1 ran
/// fine and reported 1, while a runner that refuses its profile exits with its
/// own reserved status and prints a diagnostic. Treating the second as the first
/// would report "your command failed" for a command that never started — and
/// treating the first as the second would discard real output. Exit status alone
/// cannot tell them apart, so the rule combines an optional exit gate with a
/// signature, and filters expected informational lines out first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerFailureRule {
    /// Nonzero exit codes on which this rule may match; `None` permits any.
    pub allowed_exit_codes: Option<Vec<i32>>,
    /// Non-empty substrings identifying a fatal diagnostic on one stderr line.
    pub fatal_signatures: Vec<String>,
    /// Benign stderr lines excluded by exact full-line equality before matching.
    pub informational_lines: Vec<String>,
}

impl RunnerFailureRule {
    /// A rule that matches on any nonzero exit.
    ///
    /// Correct for `bwrap` and `sandbox-exec`, whose public contracts do not
    /// reserve a failure status — so the signature alone has to carry the claim.
    pub fn any_exit(signatures: &[&str]) -> Self {
        Self {
            allowed_exit_codes: None,
            fatal_signatures: signatures.iter().map(|s| s.to_string()).collect(),
            informational_lines: vec![],
        }
    }
}

/// Fatal runner evidence, retained for the infrastructure-error detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerFailureMatch {
    /// The original stderr line that matched a fatal signature.
    pub detail: String,
}

/// Classify a settled process against the runner-failure rules.
///
/// Each rule requires a nonzero exit, its optional exit-code gate, and a fatal
/// signature on one stderr line *after* exact informational lines are excluded.
/// Returns the first matching line, or `None` when the evidence is insufficient.
///
/// This is `classifyRunnerFailure` from `bash-sandbox/src/helpers.ts`, and the
/// details are load-bearing rather than incidental:
///
/// - **A signal death is not a runner failure.** `exitCode === null` returns
///   `None` immediately, so a child killed by the timeout is never reported as
///   "the sandbox did not start".
/// - **Informational lines are removed before matching, not after.** A run that
///   prints the partial-enforcement notice and then fails for real must still be
///   classified by the *real* failure, so the notice is filtered line by line
///   rather than the whole stderr being rejected.
/// - **Whitespace-only signatures are ignored, not the rule.** Filtering them out
///   keeps a valid signature beside an empty one active; the obvious
///   `signatures.iter().all(non_empty)` would disable the rule.
pub fn classify_runner_failure(
    exit_code: Option<i32>,
    stderr: &str,
    rules: &[RunnerFailureRule],
) -> Option<RunnerFailureMatch> {
    let exit_code = exit_code.filter(|c| *c != 0)?;
    for rule in rules {
        if let Some(allowed) = &rule.allowed_exit_codes
            && !allowed.contains(&exit_code)
        {
            continue;
        }
        let informational: Vec<String> = rule
            .informational_lines
            .iter()
            .map(|l| l.to_lowercase())
            .collect();
        let fatal: Vec<String> = rule
            .fatal_signatures
            .iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_lowercase())
            .collect();
        for line in stderr.split('\n') {
            let lowered = line.trim_end_matches('\r').to_lowercase();
            if informational.contains(&lowered) {
                continue;
            }
            if fatal.iter().any(|s| lowered.contains(s)) {
                return Some(RunnerFailureMatch {
                    detail: line.to_string(),
                });
            }
        }
    }
    None
}

/// Match a nonzero exit against case-insensitive stderr signatures.
///
/// `matchesSignature` from the same module: a signal death (`None`) and a zero
/// exit both match nothing.
pub fn matches_signature(exit_code: Option<i32>, stderr: &str, signatures: &[&str]) -> bool {
    if exit_code.is_none() || exit_code == Some(0) {
        return false;
    }
    let lowered = stderr.to_lowercase();
    signatures
        .iter()
        .any(|s| lowered.contains(&s.to_lowercase()))
}

/// One confined execution, ready to spawn.
///
/// The result of [`confine`]: the argv as wrapped, plus every fact needed to
/// interpret what the run produces. Carrying the metadata *with* the argv is the
/// point — the classification depends on which runner was chosen, and a caller
/// that had to re-derive that after the fact could get it wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinedArgv {
    pub argv: Vec<String>,
    pub enforcement: Enforcement,
    pub denial_signatures: Vec<&'static str>,
    pub runner_failure_rules: Vec<RunnerFailureRule>,
}

/// Why confinement could not be established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub mode: Mode,
    /// The runner's own failure, when a runner existed but did not work.
    pub runner_detail: Option<String>,
}

impl Unavailable {
    /// The model-facing message, and the corpus's verbatim text.
    ///
    /// Recorded in `missing-sandbox-runner`, which is the one message in this
    /// module that a snapshot fixes byte for byte. It names the mode, refuses
    /// explicitly, lists what would fix it, and names the one escape hatch
    /// (`danger-full-access`) — because a model that cannot run *anything* has no
    /// way to make progress unless it is told what the alternatives are.
    pub fn message(&self) -> String {
        let mut text = format!(
            "sandbox mode \"{}\" is requested but no sandbox backend is usable on this host; \
             refusing to run the command unconfined. Install bubblewrap or run a \
             Landlock-enforcing kernel (Linux), ensure sandbox-exec is usable (macOS), or \
             ensure the ACL restricted-token runner can start (Windows) — otherwise switch \
             the consumer to danger-full-access.",
            self.mode.as_str()
        );
        if let Some(detail) = &self.runner_detail {
            text.push_str(&format!(" Runner failure: {detail}"));
        }
        text
    }
}

/// The verdict of walking a platform's runner chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// This runner confines, at this completeness.
    Confined(Runner, Enforcement),
    /// No runner on this platform works.
    Unavailable,
}

/// Walk a runner chain, given the platforms's probe verdicts.
///
/// `probe` answers "is this runner usable" for one runner — a pure input,
/// because the machine cannot spawn (see [`Probe`]). The walk is upstream's:
/// a chain of one is selected *without* probing (a sole candidate's own
/// execution-time refusal is the fail-closed end), while a longer chain is
/// probed in preference order and the first usable runner wins.
///
/// A single-candidate chain's enforcement comes from
/// [`Runner::enforcement`] because there is no probe to report otherwise; a
/// probed chain reports what the probe found, which is how the Landlock launcher
/// can be selected as `partial` on an older kernel ABI.
pub fn select_runner(
    chain: &[Runner],
    probe: &mut dyn FnMut(Runner) -> Option<Enforcement>,
) -> Selection {
    if chain.len() == 1 {
        let runner = chain[0];
        return Selection::Confined(runner, runner.enforcement());
    }
    for runner in chain {
        if let Some(enforcement) = probe(*runner) {
            return Selection::Confined(*runner, enforcement);
        }
    }
    Selection::Unavailable
}

/// What a machine needs to know about a probe before it can choose.
pub type Probe = Option<Enforcement>;

/// The profile arguments for one runner and policy — the confinement itself.
///
/// Each runner speaks its own dialect, and the dialects are **not**
/// interchangeable translations of one another. `bwrap` expresses the policy as
/// mounts (a read-only bind of `/`, then a read-write bind of the writable
/// roots); the Landlock launcher expresses it as an allow-list of grant roots in
/// two classes; Seatbelt expresses it as SBPL rules. Writing them as one
/// parameterized profile would lose the distinction that makes each correct.
///
/// The arms that matter for the mode vocabulary:
///
/// - `read-only` grants **only** `/dev/null` for write. That is the mode's whole
///   meaning: required sinks, nothing else.
/// - `workspace-write` adds the workspace root and a temp area.
/// - `danger-full-access` never reaches here — [`confine`] returns the argv
///   unwrapped for it, because a profile that grants everything is a profile
///   whose presence would mislead a reader into thinking something was enforced.
pub fn profile_args(runner: Runner, mode: Mode, workspace_root: &str) -> Vec<String> {
    match runner {
        Runner::Bwrap => {
            let mut args: Vec<String> = [
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--unshare-pid",
                "--proc",
                "/proc",
                "--die-with-parent",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            if mode == Mode::WorkspaceWrite {
                args.push("--tmpfs".to_string());
                args.push("/tmp".to_string());
                args.push("--bind".to_string());
                args.push(workspace_root.to_string());
                args.push(workspace_root.to_string());
            }
            args
        }
        Runner::Landlock => {
            // The launcher's argv grammar is `--ro <path>... --rw <path>... --
            // <argv>...`, so the profile is an argument *prefix*, not a
            // pre-separator fragment like bwrap's.
            let mut read_write = vec!["/dev/null".to_string()];
            if mode == Mode::WorkspaceWrite {
                read_write.push("/tmp".to_string());
                read_write.push(workspace_root.to_string());
            }
            let mut args = vec!["--ro".to_string(), "/".to_string()];
            for root in read_write {
                args.push("--rw".to_string());
                args.push(root);
            }
            args
        }
        Runner::Seatbelt => {
            let mut forms = vec![
                "(version 1)".to_string(),
                "(allow default)".to_string(),
                "(deny file-write*)".to_string(),
                format!("(allow file-write* (literal {}))", sbpl_string("/dev/null")),
            ];
            // The writable roots come from the *shared* helper the in-process
            // fence also uses, so the Seatbelt grant and the containment fence
            // cannot drift apart — a divergence would mean the kernel and the
            // policy checker disagree about what is writable, which is the one
            // inconsistency a confinement story cannot afford.
            let roots = writable_roots_for(mode, workspace_root);
            if !roots.is_empty() {
                let subpaths: Vec<String> = roots
                    .iter()
                    .map(|root| format!("(subpath {})", sbpl_string(root)))
                    .collect();
                forms.push(format!("(allow file-write* {})", subpaths.join(" ")));
            }
            vec!["-p".to_string(), forms.join(" ")]
        }
        Runner::WindowsAcl => {
            // The restricted-token runner takes the grants as flags rather than
            // a profile document: a workspace flag and a temp flag, both
            // required even at `read-only` (the runner creates the token from
            // them). See `windowsAclRunnerArgs` in the provider.
            let mut args = vec![
                "--workspace".to_string(),
                workspace_root.to_string(),
                "--temp".to_string(),
                "/tmp".to_string(),
                "--mode".to_string(),
                mode.as_str().to_string(),
            ];
            args.dedup();
            args
        }
    }
}

/// Quote one path as an SBPL string literal.
///
/// Backslash first, then quote: the other order would double-escape the
/// backslashes introduced by the quote replacement.
fn sbpl_string(path: &str) -> String {
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Wrap an argv for confinement under one policy.
///
/// This is the whole seam, and it has exactly two outcomes: a wrapped argv, or a
/// failure. There is deliberately no third arm returning the argv unchanged —
/// that is the silent unconfined passthrough the service contract forbids.
///
/// `danger-full-access` is *not* that forbidden arm: it is the mode whose
/// documented meaning is "bypasses confinement", so returning the argv unchanged
/// is what it asks for and the caller knows it. The forbidden thing is doing so
/// for a mode that promised confinement.
pub fn confine(
    argv: &[String],
    mode: Mode,
    workspace_root: &str,
    selection: &Selection,
) -> Result<ConfinedArgv, Unavailable> {
    if mode == Mode::DangerFullAccess {
        return Ok(ConfinedArgv {
            argv: argv.to_vec(),
            enforcement: Enforcement::Full,
            denial_signatures: vec![],
            runner_failure_rules: vec![],
        });
    }
    let Selection::Confined(runner, enforcement) = selection else {
        return Err(Unavailable {
            mode,
            runner_detail: None,
        });
    };
    let mut wrapped = match runner {
        // bwrap and Seatbelt take a profile fragment then a `--` separator:
        // everything after the separator is the command, so the profile cannot
        // swallow an argument the caller passed.
        Runner::Bwrap | Runner::Seatbelt => {
            let mut v = vec![runner.program().to_string()];
            v.extend(profile_args(*runner, mode, workspace_root));
            v.push("--".to_string());
            v
        }
        // The Landlock launcher's grammar ends its grants with the same `--`,
        // but its program is the launcher itself.
        Runner::Landlock => {
            let mut v = vec![runner.program().to_string()];
            v.extend(profile_args(*runner, mode, workspace_root));
            v.push("--".to_string());
            v
        }
        // The Windows runner takes its grants as flags and then the command
        // directly, with no separator.
        Runner::WindowsAcl => {
            let mut v = vec![runner.program().to_string()];
            v.extend(profile_args(*runner, mode, workspace_root));
            v
        }
    };
    wrapped.extend(argv.iter().cloned());
    Ok(ConfinedArgv {
        argv: wrapped,
        enforcement: *enforcement,
        denial_signatures: runner.denial_signatures().to_vec(),
        runner_failure_rules: runner.failure_rules(),
    })
}

/// The `tool/result` payload for a run that never started.
///
/// Distinct from a denial: a denial means the command ran and the kernel refused
/// an operation, while this means no command ran at all. Both are errors, but
/// only one of them is worth retrying with a different command, and the model
/// cannot tell which from the text alone unless the text says so.
pub fn runner_failure_outcome(mode: Mode, detail: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": Unavailable { mode, runner_detail: Some(detail.to_string()) }.message() }],
        "isError": true,
    })
}

/// The sandbox facts a tool result carries, mirroring `ShellSandboxInfo`.
///
/// Present only on a confined run: an unconfined process has no facts to report,
/// and reporting `enforcement: full` for one would be a lie. The `denied` flag is
/// what turns a nonzero exit into an actionable message — "the sandbox refused
/// this" rather than "your command failed" — so it is computed from the selected
/// runner's dialect against the actual stderr.
pub fn sandbox_info(
    confined: &ConfinedArgv,
    mode: Mode,
    exit_code: Option<i32>,
    stderr: &str,
) -> Value {
    let runner_failed =
        classify_runner_failure(exit_code, stderr, &confined.runner_failure_rules).is_some();
    // Runner failure outranks denial because the command did not run: if the
    // sandbox never started, nothing was denied.
    let denied =
        !runner_failed && matches_signature(exit_code, stderr, &confined.denial_signatures);
    json!({
        "mode": mode.as_str(),
        "denied": denied,
        "enforcement": confined.enforcement.as_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The missing-runner message is fixed byte for byte by
    /// `missing-sandbox-runner`'s `tool/result`. It is the one string in this
    /// module that a snapshot pins, so it is asserted against the recorded text
    /// rather than a paraphrase.
    #[test]
    fn the_unavailable_message_is_the_recorded_one() {
        let detail = "Error: spawn {{cwd}}/.dsh-missing-sandbox-runner ENOENT";
        let message = Unavailable {
            mode: Mode::ReadOnly,
            runner_detail: Some(detail.to_string()),
        }
        .message();
        assert_eq!(
            message,
            "sandbox mode \"read-only\" is requested but no sandbox backend is usable on this \
             host; refusing to run the command unconfined. Install bubblewrap or run a \
             Landlock-enforcing kernel (Linux), ensure sandbox-exec is usable (macOS), or \
             ensure the ACL restricted-token runner can start (Windows) — otherwise switch \
             the consumer to danger-full-access. Runner failure: Error: spawn \
             {{cwd}}/.dsh-missing-sandbox-runner ENOENT"
        );
    }

    /// **A confined mode never returns the argv unwrapped.**
    ///
    /// This is the service contract's "silent unconfined passthrough is
    /// forbidden", and it is the property whose violation is worst: a caller that
    /// asked for `read-only` and silently got an unconfined process believes it
    /// was confined, so it will trust the result. Asserted over *every* confined
    /// mode and *every* unavailable selection.
    #[test]
    fn a_confined_mode_never_passes_the_argv_through() {
        let argv = vec!["bash".to_string(), "-c".to_string(), "rm -rf /".to_string()];
        for mode in [Mode::ReadOnly, Mode::WorkspaceWrite] {
            let err = confine(&argv, mode, "/w", &Selection::Unavailable).unwrap_err();
            assert_eq!(err.mode, mode);
            // And the failure names the mode that was refused.
            assert!(err.message().contains(mode.as_str()));
        }
    }

    /// `danger-full-access` is the mode whose *documented meaning* is that
    /// confinement is bypassed, so an unchanged argv is the correct answer rather
    /// than the forbidden passthrough — and it reports no denial dialect,
    /// because nothing can deny.
    #[test]
    fn danger_full_access_bypasses_confinement_by_contract() {
        let argv = vec!["bash".to_string(), "-c".to_string(), "true".to_string()];
        let confined = confine(&argv, Mode::DangerFullAccess, "/w", &Selection::Unavailable)
            .expect("this mode does not need a runner");
        assert_eq!(confined.argv, argv);
        assert!(confined.denial_signatures.is_empty());
        assert!(!matches_signature(Some(1), "permission denied", &[]));
    }

    /// The chain is probed only when it has more than one candidate.
    ///
    /// A sole candidate is selected without probing — its own execution-time
    /// refusal is the fail-closed end — so asking for a probe verdict that does
    /// not exist must not change the outcome. On Linux the first usable runner
    /// wins, in preference order.
    #[test]
    fn a_sole_candidate_is_selected_without_a_probe() {
        let mut probes = 0;
        let selection = select_runner(Runner::chain("macos"), &mut |_| {
            probes += 1;
            None
        });
        assert_eq!(
            selection,
            Selection::Confined(Runner::Seatbelt, Enforcement::Full)
        );
        assert_eq!(probes, 0, "a chain of one is never probed");

        // Linux prefers bwrap, and a partial landlock verdict is reported as
        // such rather than rounded up to full.
        let selection = select_runner(Runner::chain("linux"), &mut |r| match r {
            Runner::Bwrap => None,
            Runner::Landlock => Some(Enforcement::Partial),
            _ => None,
        });
        assert_eq!(
            selection,
            Selection::Confined(Runner::Landlock, Enforcement::Partial)
        );

        // An unknown platform has no chain, so it fails closed.
        assert_eq!(
            select_runner(Runner::chain("freebsd"), &mut |_| None),
            Selection::Unavailable
        );
    }

    /// **A runner that did not start is not a command that failed.**
    ///
    /// The Landlock rule is exit-gated on 125 precisely so a confined command
    /// that exits 125 for its own reasons is not misread as a broken sandbox —
    /// and the partial-enforcement notice is excluded so a *successful* older-ABI
    /// run is not either.
    #[test]
    fn runner_failure_is_exit_gated_and_filters_informational_lines() {
        let rules = Runner::Landlock.failure_rules();

        // The launcher's real failure dialect, at its reserved exit.
        let matched = classify_runner_failure(
            Some(125),
            "landlock-run: usage error: unknown argument\n",
            &rules,
        );
        assert_eq!(
            matched.map(|m| m.detail),
            Some("landlock-run: usage error: unknown argument".to_string())
        );

        // The same line at a *different* exit is a command's own output, not the
        // runner's failure.
        assert!(classify_runner_failure(Some(1), "landlock-run: usage error\n", &rules).is_none());

        // A successful older-ABI run prints the notice and must not match.
        assert!(
            classify_runner_failure(Some(1), &format!("{PARTIAL_ENFORCEMENT_NOTICE}\n"), &rules)
                .is_none()
        );

        // The notice does not shield a *real* failure beside it: informational
        // lines are removed one by one, not as a whole-stderr switch.
        let both =
            format!("{PARTIAL_ENFORCEMENT_NOTICE}\nlandlock-run: cannot open rule path: /nope\n");
        assert_eq!(
            classify_runner_failure(Some(125), &both, &rules).map(|m| m.detail),
            Some("landlock-run: cannot open rule path: /nope".to_string())
        );

        // A signal death carries no evidence either way.
        assert!(classify_runner_failure(None, "landlock-run: x\n", &rules).is_none());
        // Nor does a clean exit, whatever it printed.
        assert!(classify_runner_failure(Some(0), "landlock-run: x\n", &rules).is_none());
    }

    /// The two signature families are independent: a runner failure is not a
    /// denial, and `sandbox_info` says so.
    ///
    /// The corpus's `partial-landlock-child-failure` is exactly this shape — a
    /// confined `false` that exits 1 and reports `denied: false`, because exiting
    /// nonzero is the command's business and the sandbox allowed it.
    #[test]
    fn a_runner_failure_outranks_a_denial_and_a_plain_failure_is_neither() {
        let argv = vec!["bash".to_string(), "-c".to_string(), "false".to_string()];
        let selection = Selection::Confined(Runner::Landlock, Enforcement::Partial);
        let confined = confine(&argv, Mode::ReadOnly, "/w", &selection).unwrap();

        // A plain nonzero exit: not denied, not a runner failure.
        let info = sandbox_info(&confined, Mode::ReadOnly, Some(1), "");
        assert_eq!(info["denied"], json!(false));
        assert_eq!(info["enforcement"], json!("partial"));
        assert_eq!(info["mode"], json!("read-only"));

        // The kernel's own dialect: denied.
        let info = sandbox_info(
            &confined,
            Mode::ReadOnly,
            Some(1),
            "bash: /w/x: Permission denied\n",
        );
        assert_eq!(info["denied"], json!(true));

        // The runner's dialect at its reserved exit: not a denial, because the
        // command never ran.
        let info = sandbox_info(
            &confined,
            Mode::ReadOnly,
            Some(125),
            "landlock-run: usage error\n",
        );
        assert_eq!(info["denied"], json!(false));
    }

    /// Each runner's profile grants exactly what its mode means.
    ///
    /// `read-only` grants `/dev/null` and nothing else — the mode's entire
    /// meaning — and `workspace-write` adds the workspace and a temp area. The
    /// assertion is on the *absence* for read-only as much as the presence for
    /// workspace-write, because a profile that leaked a writable root into
    /// read-only would be a silent widening.
    #[test]
    fn profiles_grant_what_the_mode_means() {
        for runner in [Runner::Bwrap, Runner::Landlock, Runner::Seatbelt] {
            let read_only = profile_args(runner, Mode::ReadOnly, "/w").join(" ");
            assert!(
                !read_only.contains("/w"),
                "{} must not grant the workspace at read-only: {read_only}",
                runner.as_str()
            );

            let workspace = profile_args(runner, Mode::WorkspaceWrite, "/w").join(" ");
            assert!(
                workspace.contains("/w"),
                "{} must grant the workspace at workspace-write: {workspace}",
                runner.as_str()
            );
            assert!(
                workspace.contains("/tmp"),
                "{} must grant a temp area at workspace-write: {workspace}",
                runner.as_str()
            );
        }
    }

    /// Every wrapped argv keeps the command behind a separator, so a profile can
    /// never swallow an argument the caller passed.
    ///
    /// bwrap's profiles are built from caller-independent constants, but the
    /// property is asserted rather than argued because the failure is invisible:
    /// a missing `--` would silently append the command to the profile, and bwrap
    /// would report a usage error the model would read as its own command being
    /// wrong.
    #[test]
    fn the_wrapped_argv_keeps_the_command_after_a_separator() {
        let argv = vec!["bash".to_string(), "-c".to_string(), "echo hi".to_string()];
        for runner in [Runner::Bwrap, Runner::Landlock] {
            let selection = Selection::Confined(runner, Enforcement::Full);
            let confined = confine(&argv, Mode::ReadOnly, "/w", &selection).unwrap();
            let sep = confined
                .argv
                .iter()
                .position(|a| a == "--")
                .unwrap_or_else(|| panic!("{} needs a separator", runner.as_str()));
            assert_eq!(
                &confined.argv[sep + 1..],
                argv.as_slice(),
                "{} must pass the command through verbatim",
                runner.as_str()
            );
            assert_eq!(confined.argv[0], runner.program());
        }
    }

    /// The SBPL escaping order matters: backslashes are escaped first, or the
    /// quotes' own backslashes get doubled.
    #[test]
    fn sbpl_quoting_escapes_backslashes_before_quotes() {
        assert_eq!(sbpl_string("/plain"), "\"/plain\"");
        assert_eq!(sbpl_string("/a\"b"), "\"/a\\\"b\"");
        assert_eq!(sbpl_string("/a\\b"), "\"/a\\\\b\"");
        assert_eq!(sbpl_string("/a\\\"b"), "\"/a\\\\\\\"b\"");
    }

    /// Windows claims `partial` and the others claim `full` — the one place the
    /// enforcement vocabulary is not uniform, and it is a claim about the
    /// backend rather than a hedge.
    #[test]
    fn enforcement_is_per_runner_and_others_claim_full() {
        assert_eq!(Runner::WindowsAcl.enforcement(), Enforcement::Partial);
        for runner in [Runner::Bwrap, Runner::Landlock, Runner::Seatbelt] {
            assert_eq!(runner.enforcement(), Enforcement::Full);
        }
    }

    // -----------------------------------------------------------------------
    // The real kernel.
    //
    // Everything above is the pure half, and it is worth being explicit that the
    // pure half cannot on its own establish the claim this module exists for:
    // that a confined process is *actually* confined. A profile that is spelled
    // wrong passes every assertion above and enforces nothing. So these tests
    // spawn a real `bwrap` and check the kernel's answer.
    //
    // They self-skip when `bwrap` is absent — the honest fallback, since a host
    // without it is not a host where the claim is false, only one where it is
    // untested. What they must NOT do is skip by asserting nothing: the skip is
    // printed so a reader can see the difference between "passed" and "did not
    // run".
    // -----------------------------------------------------------------------

    /// Spawn a confined argv through the real driver effect and return
    /// `(exit_code, stdout, stderr)`.
    ///
    /// Routed through [`crate::driver::realize_with`] rather than a local
    /// `Command` so the test exercises the *same* path production uses — the
    /// effect, the driver arm, the reader threads, the timeout. A test that
    /// reimplemented the spawn would pass while the real arm was broken.
    fn run_confined(confined: &ConfinedArgv, workdir: &str) -> (Option<i32>, String, String) {
        let outcome = crate::driver::realize_with(
            vocoder_cordis::RealizeRequest::ProcessExec {
                argv: confined.argv.clone(),
                workdir: Some(workdir.to_string()),
                // A minimal environment: bwrap needs PATH to find the payload,
                // and nothing else. Passing the server's environment would make
                // this test depend on it.
                env: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
                timeout_ms: Some(30_000),
                stdout_max_bytes: Some(64 * 1024),
                stdin: None,
            },
            &mut |_| {},
        );
        match outcome {
            Some(vocoder_cordis::EffectResult::ProcessDone {
                exit_code,
                stdout,
                stderr,
                ..
            }) => (exit_code, stdout, stderr),
            other => panic!("expected a settled process, got {other:?}"),
        }
    }

    /// Whether `bwrap` can create a profile here, tested the way upstream tests
    /// it: run the real read-only profile around `true` and see if the kernel
    /// accepts it.
    ///
    /// A functional probe rather than an existence check, because bwrap can be
    /// installed and still unusable — an unprivileged-userns-disabled kernel is
    /// the common case — and a test that only checked for the binary would then
    /// fail on a working host's absence of permission, which reads as a bug in
    /// this module.
    fn bwrap_usable() -> bool {
        std::process::Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--unshare-pid",
                "--proc",
                "/proc",
                "--die-with-parent",
                "--",
                "true",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// **`read-only` is enforced by the kernel: a confined write is refused and
    /// the file does not appear.**
    ///
    /// The assertion that matters is the second one. A denial message alone can
    /// be produced by a profile that did not enforce anything — the payload's own
    /// shell reporting a failure — so the test checks the *observable world*: the
    /// target path must not exist afterwards. That is the difference between
    /// testing the message and testing the confinement.
    #[test]
    fn the_kernel_refuses_a_write_under_read_only() {
        if !bwrap_usable() {
            eprintln!("SKIP: bwrap is not usable on this host; kernel enforcement unverified");
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        let workdir = dir.path().to_string_lossy().to_string();
        let target = format!("{workdir}/must-not-exist.txt");

        let argv = vec![
            "bash".to_string(),
            "-c".to_string(),
            format!("echo written > '{target}'"),
        ];
        let selection = Selection::Confined(Runner::Bwrap, Enforcement::Full);
        let confined =
            confine(&argv, Mode::ReadOnly, &workdir, &selection).expect("a runner exists");

        let (code, _out, err) = run_confined(&confined, &workdir);

        // **The file is not there.** This is the real claim.
        assert!(
            !std::path::Path::new(&target).exists(),
            "read-only confinement let a write through: {err:?}"
        );
        // The kernel refused it, and the refusal speaks bwrap's dialect — which
        // is what makes `sandbox_info` able to classify it as a denial.
        assert_ne!(code, Some(0), "the confined write should have failed");
        assert!(
            matches_signature(code, &err, Runner::Bwrap.denial_signatures()),
            "bwrap's denial dialect should match its own refusal: {err:?}"
        );
        let info = sandbox_info(&confined, Mode::ReadOnly, code, &err);
        assert_eq!(
            info["denied"],
            json!(true),
            "the refusal must classify as a denial, not as a broken runner: {err:?}"
        );
    }

    /// **`workspace-write` grants the workspace and still refuses outside it.**
    ///
    /// Both halves in one test because either alone is satisfiable by a wrong
    /// profile: a profile that grants nothing passes the outside-refusal half
    /// while making the mode useless, and one that grants everything passes the
    /// inside-write half while confining nothing.
    #[test]
    fn the_kernel_grants_the_workspace_and_refuses_beyond_it() {
        if !bwrap_usable() {
            eprintln!("SKIP: bwrap is not usable on this host; kernel enforcement unverified");
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        let workdir = dir.path().to_string_lossy().to_string();
        let inside = format!("{workdir}/inside.txt");
        // A path that certainly exists and is certainly outside the workspace.
        let outside = "/var/tmp/dsh-confinement-probe-should-not-exist.txt";

        let argv = vec![
            "bash".to_string(),
            "-c".to_string(),
            format!("echo ok > '{inside}'; echo no > '{outside}' 2>/dev/null; true"),
        ];
        let selection = Selection::Confined(Runner::Bwrap, Enforcement::Full);
        let confined =
            confine(&argv, Mode::WorkspaceWrite, &workdir, &selection).expect("a runner exists");

        let (_code, _out, _err) = run_confined(&confined, &workdir);

        // The point of the mode: the workspace is writable.
        assert!(
            std::path::Path::new(&inside).exists(),
            "workspace-write must grant the workspace root"
        );
        assert_eq!(
            std::fs::read_to_string(&inside).unwrap().trim(),
            "ok",
            "the confined write's contents must land"
        );
        // And the fence still holds outside it.
        assert!(
            !std::path::Path::new(outside).exists(),
            "workspace-write must not grant a path outside the workspace"
        );
    }

    /// The denial a confined process produces is classified as a **denial**, not
    /// as a runner failure — the distinction postmortem 0004 was written about.
    ///
    /// Pinned against the kernel rather than a fixture because the fixture and
    /// the kernel are different evidence: the fixture says the *classifier* is
    /// right, and this says the classifier meets a real bwrap's real stderr. The
    /// recorded dialect (`read-only file system`) is asserted to be what the
    /// kernel actually prints, so a bwrap release that changes its wording makes
    /// this test fail rather than silently classifying every denial as unknown.
    #[test]
    fn a_real_denial_is_not_a_runner_failure() {
        if !bwrap_usable() {
            eprintln!("SKIP: bwrap is not usable on this host; kernel dialect unverified");
            return;
        }
        let dir = tempfile::tempdir().expect("dir");
        let workdir = dir.path().to_string_lossy().to_string();
        let argv = vec![
            "bash".to_string(),
            "-c".to_string(),
            format!("echo x > {workdir}/nope.txt"),
        ];
        let selection = Selection::Confined(Runner::Bwrap, Enforcement::Full);
        let confined =
            confine(&argv, Mode::ReadOnly, &workdir, &selection).expect("a runner exists");
        let (code, _out, err) = run_confined(&confined, &workdir);

        assert!(
            classify_runner_failure(code, &err, &confined.runner_failure_rules).is_none(),
            "a bwrap denial must not read as the runner failing: {err:?}"
        );
        assert!(
            looks_like_a_denial(code, &err, &confined),
            "the recorded dialect must match a real bwrap: {err:?}"
        );
    }

    /// Whether a settled run carries the selected runner's denial dialect.
    fn looks_like_a_denial(code: Option<i32>, err: &str, confined: &ConfinedArgv) -> bool {
        matches_signature(code, err, &confined.denial_signatures)
    }

    /// **A missing runner is a spawn failure, and the message is the recorded
    /// one.**
    ///
    /// This is `missing-sandbox-runner`'s scenario reproduced without the
    /// snapshot machinery: a runner path that does not exist. The two claims are
    /// that the spawn failure surfaces as an error rather than a silent success,
    /// and that the text a model would read is the fail-closed message naming
    /// every remedy.
    #[test]
    fn a_runner_that_cannot_spawn_fails_closed_with_the_recorded_message() {
        let argv = ["true".to_string()];
        // The fixture's argv: the runner is an executable under the workspace
        // that was never created.
        let missing = "/nonexistent-workspace/.dsh-missing-sandbox-runner";
        let confined = ConfinedArgv {
            argv: std::iter::once(missing.to_string())
                .chain(argv.iter().cloned())
                .collect(),
            enforcement: Enforcement::Full,
            denial_signatures: vec!["permission denied"],
            runner_failure_rules: vec![RunnerFailureRule::any_exit(&["snapshot-runner: "])],
        };

        let outcome = crate::driver::realize_with(
            vocoder_cordis::RealizeRequest::ProcessExec {
                argv: confined.argv.clone(),
                workdir: None,
                env: vec![],
                timeout_ms: Some(5_000),
                stdout_max_bytes: None,
                stdin: None,
            },
            &mut |_| {},
        );
        // A failed spawn is an *effect* failure, not a settled process: nothing
        // ran, so there is no exit code to classify. That is the mechanism that
        // makes "the runner never started" distinguishable from "the command
        // failed" — the first never reaches the classifier at all.
        match outcome {
            Some(vocoder_cordis::EffectResult::Failed(e)) => {
                assert!(
                    e.message().contains(".dsh-missing-sandbox-runner"),
                    "the spawn failure must name the runner: {}",
                    e.message()
                );
            }
            other => panic!("a missing runner must fail the spawn, got {other:?}"),
        }

        let message = Unavailable {
            mode: Mode::ReadOnly,
            runner_detail: None,
        }
        .message();
        assert!(message.contains("no sandbox backend is usable"));
        assert!(message.contains("refusing to run the command unconfined"));
    }

    /// `danger-full-access` runs unconfined, and the process really does write
    /// outside the workspace — which is what the mode promises, asserted so the
    /// bypass is a tested fact rather than an assumption.
    #[test]
    fn danger_full_access_really_is_unconfined() {
        let dir = tempfile::tempdir().expect("dir");
        let workdir = dir.path().to_string_lossy().to_string();
        let argv = vec![
            "bash".to_string(),
            "-c".to_string(),
            format!("echo free > {workdir}/unconfined.txt"),
        ];
        let confined = confine(
            &argv,
            Mode::DangerFullAccess,
            &workdir,
            &Selection::Unavailable,
        )
        .expect("this mode needs no runner");
        let (code, _out, _err) = run_confined(&confined, &workdir);
        assert_eq!(code, Some(0));
        assert!(
            std::path::Path::new(&workdir)
                .join("unconfined.txt")
                .exists()
        );
    }
}
