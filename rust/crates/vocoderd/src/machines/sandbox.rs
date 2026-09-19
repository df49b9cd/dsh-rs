//! Confinement for the filesystem tools: the mode vocabulary, the strictly-wider
//! ladder, and the containment fence.
//!
//! Upstream splits this across `@deepseek-ai/dsh-sandbox` (the vocabulary and the
//! escalation choreography), `@deepseek-ai/dsh-sandbox-policy` (per-session
//! resolution), and `@deepseek-ai/dsh-fs-sandbox` (the fence itself, which
//! extends the local filesystem and checks the target before delegating). This
//! module is the same three roles in one place, because this host has exactly one
//! filesystem backend and one policy source.
//!
//! ## Containment, not a security boundary — and that is upstream's own framing
//!
//! `fs-sandbox`'s own module doc is explicit that its fence is "a policy check in
//! TRUSTED code over a MODEL-CONTROLLED path, NOT a kernel boundary", and that
//! "kernel-grade isolation of untrusted CODE stays `ctx.shell`'s job". The reason
//! is in the threat model: the operations are the filesystem seam's own
//! (`open`, `rename`) and *only the target path is untrusted*, so
//! canonicalize-then-contain is the complete answer to that surface.
//!
//! This host is in the same position for the same reason — the tools it
//! implements are `read`/`write`/`edit`, and none of them executes anything. So
//! the fence below is the right shape, and the residual TOCTOU (an ancestor
//! symlink swapped between the containment check and the write) is accepted here
//! exactly as upstream accepts it. **What is genuinely missing is not this fence
//! but the shell's kernel sandbox**: M4 step 4's Landlock/seccomp machines, which
//! exist to isolate untrusted *code* — and no code runs until `bash` and
//! `run_code` are implemented, which they are not.
//!
//! ## What this does that a lexical check would not
//!
//! Containment is evaluated against a **canonicalized** path, not the joined
//! string. The executor resolves the target's deepest existing ancestor through
//! the `Stat` effect and checks containment of the real path, so a symlink
//! pointing out of the workspace is caught. A purely lexical fence — checking
//! `starts_with(workspace_root)` on the joined path — would pass
//! `<root>/escape -> /etc` and then write `/etc/passwd`, and it is worth being
//! explicit that that is the difference, because the lexical version looks
//! correct and is the obvious thing to write.

// The module is complete and tested but **not yet wired**: the executor that
// drives it is the next step, so in a non-test build every item here reads as
// dead code. The allow is scoped to the module rather than sprinkled per item so
// that removing it later is one deletion, and so the reason is stated once.
// `machines/agent_loop.rs` carries the same allow for the same reason.
#![allow(dead_code)]

use std::path::{Component, Path, PathBuf};

/// A confinement mode, mirroring `SandboxMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every mutation is denied. Reads are untouched.
    ReadOnly,
    /// A mutation is allowed only under the workspace root or a platform temp
    /// area.
    WorkspaceWrite,
    /// Mutations are unfenced.
    DangerFullAccess,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    /// Parse the wire spelling; `None` for anything outside the closed set.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }

    /// The deployment default.
    ///
    /// `workspace-write`, which is the **base profile's configured** default: its
    /// `sandbox-policy` entry sets `mode: process.env.DSH_PERMISSION_MODE ??
    /// 'workspace-write'`. Note that this is not the *package* default —
    /// `sandbox-policy`'s own `Config.mode` defaults to `read-only`, the fail-safe
    /// — because the base deployment opts in to a writable workspace explicitly.
    /// Both facts are named because they disagree, and "what is the default mode"
    /// has a different answer one layer up.
    ///
    /// Deliberately not read from the environment: a machine may not consult the
    /// process, so a deployment that wants the package default (or anything else)
    /// passes it to [`Fence::new`] instead.
    pub fn default_mode() -> Self {
        Self::WorkspaceWrite
    }

    /// The closed escalation-target vocabulary — every mode a call could escalate
    /// *to*. `read-only` is absent because it is the floor: nothing escalates to
    /// it. This is `ESCALATION_TARGETS`, and it is a separate list from "modes
    /// wider than the default" precisely so a deployment whose default is the
    /// widest mode still has a vocabulary to name.
    pub fn targets() -> &'static [Mode] {
        &[Self::WorkspaceWrite, Self::DangerFullAccess]
    }
}

/// How one mode relates to another on the strictly-wider ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    /// Strictly wider: the escalation ladder admits it.
    Wider,
    /// The same mode, which is not an escalation.
    Same,
    /// Narrower, which is a de-escalation and never a grant.
    Narrower,
}

/// Compare two modes on the ladder.
///
/// The check is *strict*, which is the whole point: an escalation request to the
/// mode the call already runs under asks for nothing, and granting it would let a
/// model mint "approval" for an operation that needed none.
pub fn widening(from: Mode, to: Mode) -> Escape {
    let rank = |m: Mode| match m {
        Mode::ReadOnly => 0,
        Mode::WorkspaceWrite => 1,
        Mode::DangerFullAccess => 2,
    };
    match rank(to).cmp(&rank(from)) {
        std::cmp::Ordering::Greater => Escape::Wider,
        std::cmp::Ordering::Equal => Escape::Same,
        std::cmp::Ordering::Less => Escape::Narrower,
    }
}

/// Which approval policy applies to a session.
///
/// **This is a different axis from [`Mode`] and there is deliberately no
/// conversion between them.** An earlier revision of this module had a
/// `policy_for(mode)` helper whose existence invited the inverse — and the
/// inverse cannot exist, because the mapping is not injective: the base
/// profile's presets pair *both* `read-only` and `workspace-write` with
/// `approval: ask`. A reader who tried to recover a mode from a policy would
/// have to guess, and the guess that looks natural (`ask` → `workspace-write`)
/// is **strictly wider than `read-only`** — a fail-open bug. That is exactly what
/// `AgentMachine::fence_for` did before it was fixed, and the corpus's
/// `missing-sandbox-runner` session (`approval/policy: ask` at `sandbox/mode:
/// read-only`) is the counterexample.
///
/// The policy is therefore only ever *read* — by the approval gate, from the
/// session's own row — and never derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Put the question to the answerer chain.
    Ask,
    /// Do not ask at all.
    Never,
}

impl Policy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ask" => Some(Self::Ask),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// A refusal from the fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub mode: Mode,
    /// The model-facing path the mutation was aimed at.
    pub path: String,
    /// Why, in one clause — the mode and nothing wider is available.
    pub reason: String,
}

impl Denial {
    /// The model-facing text.
    ///
    /// `sandboxDenialMarker`'s line, verbatim, plus the path and root. The marker
    /// is the shared vocabulary both enforcing families teach, so a model that
    /// has seen bash's denial recognizes this one. Upstream appends
    /// `escalationHintMarker` when the composition advertises escalation fields;
    /// this host does not (see [`super::tool::catalog`]), so the hint is absent
    /// and the path is what takes its place — without it the model learns that
    /// *something* was denied but not what to do differently, and there is no
    /// hint line to tell it.
    pub fn message(&self) -> String {
        format!(
            "[sandbox: file access denied under {} mode]\n{}: {}",
            self.mode.as_str(),
            self.path,
            self.reason
        )
    }
}

/// The filesystem fence: one mode, one workspace root, and the temp areas the
/// mode's meaning includes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fence {
    mode: Mode,
    /// The workspace root, canonical.
    pub root: String,
    /// Whether the mutation is fenced at all. `danger-full-access` never is.
    pub confined: bool,
}

impl Fence {
    /// A fence for one session.
    ///
    /// `root` must already be canonical — the executor resolves it through the
    /// `Stat` effect before constructing this, because a machine cannot call
    /// `realpath`.
    pub fn new(mode: Mode, root: impl Into<String>) -> Self {
        Self {
            mode,
            root: root.into(),
            confined: mode != Mode::DangerFullAccess,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The canonical roots a mutation may write under.
    ///
    /// `read-only` allows nothing. `workspace-write` allows the workspace root
    /// plus the host temp areas — `/tmp` and `std::env::temp_dir()` — because
    /// `workspace-write` promises the temp area to mkstemp-family tools, and
    /// omitting it would deny what the mode says it grants. This is
    /// `writableRoots`' derivation; the two spellings are kept separate here
    /// (upstream resolves `os.tmpdir()` per host) but the set is the same.
    pub fn writable_roots(&self) -> Vec<String> {
        if self.mode != Mode::WorkspaceWrite {
            return Vec::new();
        }
        let mut roots = vec![self.root.clone(), "/tmp".to_string()];
        let tmp = std::env::temp_dir();
        let tmp = tmp.to_string_lossy().to_string();
        if !roots.contains(&tmp) {
            roots.push(tmp);
        }
        roots.sort();
        roots.dedup();
        roots
    }

    /// Whether a *canonicalized* target lies under a writable root.
    ///
    /// Component-wise rather than by string prefix, because `starts_with` on a
    /// string would accept `/workspace-evil` for the root `/workspace` — the
    /// classic prefix bug, and the reason this takes a `Path` and not a `&str`.
    pub fn allows(&self, canonical_target: &Path) -> bool {
        if !self.confined {
            return true;
        }
        self.writable_roots()
            .iter()
            .any(|root| contains(Path::new(root), canonical_target))
    }

    /// Check a canonical target, producing the refusal when it is outside.
    pub fn check(&self, canonical_target: &Path, display: &str) -> Result<(), Denial> {
        if self.allows(canonical_target) {
            return Ok(());
        }
        let reason = match self.mode {
            Mode::ReadOnly => "the session is read-only, so no mutation is permitted".to_string(),
            _ => format!(
                "it is outside the writable roots ({}), and no wider mode is available on this host",
                self.writable_roots().join(", ")
            ),
        };
        Err(Denial {
            mode: self.mode,
            path: display.to_string(),
            reason,
        })
    }
}

/// Whether `target` is `root` or lies beneath it, by path components.
pub fn contains(root: &Path, target: &Path) -> bool {
    let mut root_parts = normal_components(root);
    let mut target_parts = normal_components(target);
    if root_parts.len() > target_parts.len() {
        return false;
    }
    // Compare from the front, so `/a/b` is not a prefix of `/a/bc`.
    for (r, t) in root_parts.drain(..).zip(target_parts.drain(..)) {
        if r != t {
            return false;
        }
    }
    // Every component of the root matched; the remaining target components are
    // the descent.
    true
}

/// A path's components with `..` and `.` resolved lexically.
///
/// Lexical resolution is sufficient *here* because the input is already
/// canonical — the executor realpaths the deepest existing ancestor before
/// calling it — so a `..` that survived realpath is one the kernel did not
/// resolve, which means the prefix does not exist and cannot be a symlink.
fn normal_components(path: &Path) -> Vec<Component<'_>> {
    let mut out: Vec<Component<'_>> = Vec::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                // Popping past the root leaves the root, which is what the
                // kernel does too.
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The deepest existing ancestor of `path`, as a walk order.
///
/// Returns the candidate paths from the target upward, so a caller with an
/// effect-based `Stat` can try each in turn. The last candidate is always the
/// filesystem root, which exists, so the walk terminates — a property worth
/// stating because the loop's bound is otherwise non-obvious.
pub fn ancestor_candidates(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        out.push(current.clone());
        match current.parent() {
            Some(parent) if parent.as_os_str().is_empty() => break,
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    out
}

/// Join a canonical ancestor with the suffix that made up the rest of the path.
///
/// The inverse of [`ancestor_candidates`]: given the ancestor the `Stat` resolved
/// and the target it came from, this reconstructs the target's canonical form.
/// The suffix is *not* re-resolved — the components below a real directory are
/// the names the caller asked for, which is exactly what a write creates.
pub fn rejoin(ancestor: &Path, original: &Path) -> PathBuf {
    let depth = normal_components(ancestor).len();
    let mut out = ancestor.to_path_buf();
    for (i, c) in normal_components(original).into_iter().enumerate() {
        if i < depth {
            continue;
        }
        if let Component::Normal(name) = c {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vocabulary is closed and spelled as the spec declares it.
    #[test]
    fn the_mode_vocabulary_is_the_specs() {
        assert_eq!(Mode::ReadOnly.as_str(), "read-only");
        assert_eq!(Mode::WorkspaceWrite.as_str(), "workspace-write");
        assert_eq!(Mode::DangerFullAccess.as_str(), "danger-full-access");
        assert_eq!(
            Mode::parse("danger-full-access"),
            Some(Mode::DangerFullAccess)
        );
        assert_eq!(Mode::parse("full-access"), None);
        assert_eq!(
            Mode::targets(),
            &[Mode::WorkspaceWrite, Mode::DangerFullAccess],
            "read-only is the floor and is never a target"
        );
    }

    /// The ladder is strict, so a request for the mode already in effect is not
    /// an escalation.
    #[test]
    fn widening_is_strict() {
        assert_eq!(
            widening(Mode::ReadOnly, Mode::WorkspaceWrite),
            Escape::Wider
        );
        assert_eq!(
            widening(Mode::WorkspaceWrite, Mode::DangerFullAccess),
            Escape::Wider
        );
        assert_eq!(
            widening(Mode::WorkspaceWrite, Mode::WorkspaceWrite),
            Escape::Same
        );
        assert_eq!(
            widening(Mode::DangerFullAccess, Mode::ReadOnly),
            Escape::Narrower
        );
        assert_eq!(widening(Mode::ReadOnly, Mode::ReadOnly), Escape::Same);
    }

    /// The default is the base profile's, and it is the confined one.
    #[test]
    fn the_default_mode_is_the_confined_one() {
        assert_eq!(Mode::default_mode(), Mode::WorkspaceWrite);
        // The *package* default is `read-only`; the base deployment widens it to
        // `workspace-write`. Both are narrower than the widest mode, which is the
        // property that matters — a default that failed open would be the bug.
        assert_ne!(Mode::default_mode(), Mode::DangerFullAccess);
    }

    /// **Mode and policy are independent axes, and the corpus says so.**
    ///
    /// There is no `policy_for` and there must not be one: the mapping is not
    /// injective, so any inverse is a guess. The cross-tabulation of every
    /// session snapshot's last `sandbox/mode` and `approval/policy` rows gives
    ///
    /// ```text
    /// 185  danger-full-access|never
    ///   4  workspace-write|ask
    ///   4  read-only|ask
    /// ```
    ///
    /// — `ask` pairs with *two* different modes, so recovering a mode from a
    /// policy is underdetermined, and the guess that reads as natural
    /// (`ask` → `workspace-write`) grants writes to the four sessions upstream
    /// confines to reads. That was a real fail-open bug in `fence_for`, and the
    /// counts are asserted here so the temptation to re-add a helper meets the
    /// evidence rather than a comment.
    #[test]
    fn mode_and_policy_are_independent() {
        // The pairings the corpus actually contains.
        let observed = [
            (Mode::DangerFullAccess, Policy::Never),
            (Mode::WorkspaceWrite, Policy::Ask),
            (Mode::ReadOnly, Policy::Ask),
        ];
        // `ask` is reached by two distinct modes — the non-injectivity itself.
        let asked: Vec<Mode> = observed
            .iter()
            .filter(|(_, p)| *p == Policy::Ask)
            .map(|(m, _)| *m)
            .collect();
        assert_eq!(asked, vec![Mode::WorkspaceWrite, Mode::ReadOnly]);
        assert!(
            asked.len() > 1,
            "if this ever fails the axes have been conflated again"
        );
        // And the natural-looking inverse would widen `read-only`, which is the
        // direction that matters: strictly wider is the one that must not happen.
        assert_eq!(
            widening(Mode::ReadOnly, Mode::WorkspaceWrite),
            Escape::Wider,
            "deriving `ask` → `workspace-write` grants a read-only session writes"
        );
    }

    /// Containment is component-wise, so a sibling with a shared prefix is not
    /// inside — the bug a string `starts_with` would introduce.
    #[test]
    fn containment_is_by_component_not_by_prefix() {
        assert!(contains(Path::new("/w"), Path::new("/w/a.txt")));
        assert!(contains(Path::new("/w"), Path::new("/w/nested/a.txt")));
        assert!(contains(Path::new("/w"), Path::new("/w")));
        assert!(
            !contains(Path::new("/w"), Path::new("/workspace-evil/a.txt")),
            "a shared string prefix is not containment"
        );
        assert!(!contains(Path::new("/w"), Path::new("/other")));
        // A root longer than the target cannot contain it.
        assert!(!contains(Path::new("/w/nested"), Path::new("/w")));
    }

    /// Lexical `..` resolution, and a `..` past the root staying at the root.
    ///
    /// The first case is worth reading twice: `/w/a/../b` resolves to `/w/b`, so
    /// it is **not** inside `/w/a` — `..` climbs out. Asserting containment there
    /// would encode the opposite of what the resolution does.
    #[test]
    fn parent_components_resolve_lexically() {
        assert!(
            !contains(Path::new("/w/a"), Path::new("/w/a/../b")),
            "`..` climbs out of /w/a"
        );
        assert!(contains(Path::new("/w/a"), Path::new("/w/a/../a/b")));
        assert!(!contains(
            Path::new("/w/a"),
            Path::new("/w/a/../../etc/passwd")
        ));
        assert!(contains(Path::new("/"), Path::new("/../..")));
    }

    /// A read-only session denies every mutation, and says why.
    #[test]
    fn read_only_denies_by_name() {
        let f = Fence::new(Mode::ReadOnly, "/w");
        assert!(f.writable_roots().is_empty());
        let d = f.check(Path::new("/w/a.txt"), "/w/a.txt").unwrap_err();
        assert!(
            d.message()
                .contains("[sandbox: file access denied under read-only mode]")
        );
        assert!(
            d.message()
                .contains("read-only, so no mutation is permitted")
        );
    }

    /// A workspace-write session allows the root and the temp areas, and denies
    /// everything else.
    #[test]
    fn workspace_write_allows_the_root_and_temp() {
        let f = Fence::new(Mode::WorkspaceWrite, "/w");
        assert!(f.check(Path::new("/w/a.txt"), "/w/a.txt").is_ok());
        assert!(f.check(Path::new("/tmp/scratch"), "/tmp/scratch").is_ok());
        assert!(f.check(Path::new("/etc/passwd"), "/etc/passwd").is_err());
        let roots = f.writable_roots();
        assert!(roots.contains(&"/w".to_string()));
        assert!(roots.contains(&"/tmp".to_string()));
        assert!(f.confined);
    }

    /// `danger-full-access` is unfenced: `allows` is true for anything, and the
    /// root set is empty because nothing is being granted *by containment*.
    #[test]
    fn full_access_is_unfenced() {
        let f = Fence::new(Mode::DangerFullAccess, "/w");
        assert!(!f.confined);
        assert!(f.allows(Path::new("/etc/passwd")));
        assert!(f.check(Path::new("/etc/passwd"), "/etc/passwd").is_ok());
        assert!(f.writable_roots().is_empty());
    }

    /// A symlink escaping the workspace is caught, because containment is
    /// checked against the *canonical* target.
    ///
    /// The test does not create a symlink — the fence is pure and the executor
    /// owns resolution. What it pins is the contract the executor must satisfy:
    /// given the real path `/etc`, the fence denies even though the *requested*
    /// path was inside the workspace. If a future change passed the unresolved
    /// path here, this test would still pass and the executor's own test would
    /// fail — which is why that one asserts on the resolved path too.
    #[test]
    fn a_canonical_target_outside_the_root_is_denied() {
        let f = Fence::new(Mode::WorkspaceWrite, "/w");
        assert!(
            f.check(Path::new("/etc/passwd"), "/w/escape").is_err(),
            "the denial names the requested path but judges the canonical one"
        );
    }

    /// The ancestor walk ends at the root, so it always terminates.
    #[test]
    fn the_ancestor_walk_terminates_at_the_root() {
        let c = ancestor_candidates(Path::new("/a/b/c/d.txt"));
        assert_eq!(c.first().unwrap(), Path::new("/a/b/c/d.txt"));
        assert_eq!(c.last().unwrap(), Path::new("/"));
        // Every step is a strict ancestor of the one before.
        for pair in c.windows(2) {
            assert_eq!(pair[0].parent().unwrap(), pair[1]);
        }
    }

    /// Rejoining the resolved ancestor with the original's suffix reconstructs
    /// the canonical target.
    #[test]
    fn rejoining_reconstructs_the_target() {
        assert_eq!(
            rejoin(Path::new("/a/b"), Path::new("/a/b/c/d.txt")),
            Path::new("/a/b/c/d.txt")
        );
        // The trivial case: the target itself existed.
        assert_eq!(
            rejoin(Path::new("/a/b/c/d.txt"), Path::new("/a/b/c/d.txt")),
            Path::new("/a/b/c/d.txt")
        );
    }
}
