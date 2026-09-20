//! The tool executor: runs a step's tool calls and produces the durable
//! `tool/call` / `tool/result` rows they owe.
//!
//! Upstream this is `executeToolCalls` (`dsh/packages/core/agent-loop/src/tool-calls.ts`)
//! over the tool registry's policy pipeline (`dsh/packages/core/tools/src/index.ts`).
//! This is the *emitter* the approval machine's audit pair has been waiting on:
//! before it existed, nothing dispatched `approval/request`, so the machine
//! answered a chain that never ran.
//!
//! ## Row order, which the corpus fixes and a reading of the code does not
//!
//! A tool-calling step records, in order:
//!
//! ```text
//! assistant/message          (the calls, written by the settle path)
//! tool/call                  (one per call, in model order)
//! approval/asked             (only for a widening request)
//! approval/decided
//! tool/result                (one per call, in model order)
//! step/end
//! ```
//!
//! **The `tool/call` row precedes the ask**, which is the opposite of what this
//! module's first draft asserted. `startCall` appends the call and *then* awaits
//! `prepare`, whose policy gate is what raises the question — so a denied call
//! still has its `tool/call` row, and `hook-cc-pretool-ask` shows exactly that:
//! `tool/call` → `approval/asked` → `approval/decided {rejected}` →
//! `tool/result {isError}`. Getting this backwards would lose the record of a
//! call the model made.
//!
//! ## Strict serial order, not the scheduler
//!
//! Upstream schedules exclusive calls as barriers and parallel calls through a
//! bounded rolling pool, reclassifying before each start. Every tool this host
//! implements is `isConcurrencySafe`, and the corpus shows one call per step in
//! 215 of 221 steps, so the scheduler is replaced by **strict model order, one
//! at a time**. That is a deliberate reduction: correct for this tool set, and
//! wrong for a set with an exclusive member.
//!
//! ## Read-modify-write, and the guard it carries
//!
//! `edit` is a read followed by a write, and the guard that makes it safe is
//! the same one upstream's `fs-observation-policy` enforces: the session must
//! have *observed* the target (a read, or a prior mutation's `fs/observed`
//! emit), and the write carries the observed version as its compare basis. The
//! executor records observations from every resolving `Stat` (present, or
//! absent for a create), refuses an unobserved edit or overwrite before any
//! filesystem effect beyond the stat, and — because the pipeline is serial —
//! the outstanding race narrows to the two process-level effects: the driver
//! re-stats under the write itself and refuses a changed version with
//! `fs/stale-version`. What it does not give is the *cross-process* guarantee
//! upstream's `fs-local` per-target lock gives against two hosts; this host's
//! own writes are serialized by the pump's global lock, so the guard is closed
//! here.

use serde_json::{Value, json};

use super::sandbox::Fence;
use super::tool::{
    Answer, Arguments, Call, Gate, Outcome, apply_edit, display_path, edit_outcome, edit_request,
    gate, hunk_diffs, read_outcome, read_window, unknown_tool, write_outcome, write_request,
};

/// What the executor wants done next.
///
/// The executor is pure: it never appends a row or issues an effect itself. The
/// agent machine turns these into `MachineOut`s, which is what keeps the log's
/// ordering in one owner and the executor testable without a router.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutorOut {
    /// Append this row to the log. The type travels beside the data because the
    /// log's row type is a compile-time fact of the tool, not a field the
    /// payload happens to carry.
    Row { row_type: &'static str, data: Value },
    /// Issue this effect and answer it with [`Executor::on_effect`].
    Effect(Effect),
    /// Dispatch an event; the answer arrives as a `DispatchResult`.
    Dispatch {
        name: String,
        payload: Value,
        /// Always waterfall for the approval ask.
        waterfall: bool,
    },
}

/// The effects the executor asks for.
///
/// A narrower vocabulary than [`vocoder_cordis::RealizeRequest`] on purpose: the
/// executor names what it wants in its own terms (`Stat` to resolve a target,
/// `Read` for content, `Write` for a mutation), and the agent maps them. That
/// keeps the two `Stat`-then-`Read` pipelines readable and lets a test drive the
/// executor with no effect vocabulary at all.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Resolve a target: canonical path, or `NotFound`.
    Stat { path: String },
    /// Read a file's text.
    Read { path: String },
    /// Write a file's text, guarded by the observation the tool is acting on
    /// (`WriteExpect::Version` for `edit` and for a guarded overwrite,
    /// `Absent` for a guarded create; `Any` only where the corpus shows no
    /// guard — the diff-read's rewrite does not exist, so every write the
    /// executor issues carries a guard).
    Write {
        path: String,
        contents: String,
        expect: vocoder_cordis::WriteExpect,
    },
    /// Run a model-authored command whose argv is **already wrapped** by
    /// [`super::sandbox_runner::confine`]. The whole [`ConfinedArgv`] travels
    /// rather than just the argv, because classifying what the run produces
    /// depends on which runner wrapped it — re-deriving the runner in
    /// `on_effect` would repeat a decision rather than carry it.
    Exec {
        confined: super::sandbox_runner::ConfinedArgv,
        mode: super::sandbox::Mode,
        workdir: String,
        env: Vec<(String, String)>,
        timeout_ms: u64,
    },
    /// Start a backgrounded command whose argv is already wrapped. The same
    /// fields as `Exec` minus the timeout — upstream's `run_in_background`
    /// schema says none applies — because the driver answers a pid rather
    /// than a settle, and the settle arrives later through `ExecRead`.
    ExecDetached {
        confined: super::sandbox_runner::ConfinedArgv,
        workdir: String,
        env: Vec<(String, String)>,
    },
    /// Drain a background child's unread output and report its settle state.
    /// What `job_output` maps to: non-blocking by construction because the
    /// driver is synchronous, so a `wait: true` reduction answers immediately
    /// rather than suspending the pump.
    ExecRead { pid: u32 },
    /// Ask for a background child's kill. `job_kill`'s effect; the actual
    /// settle the tool then reports is read back through `ExecRead`.
    ExecKill { pid: u32 },
}

/// Which tool a pipeline is running, once the target is resolved.
#[derive(Debug, Clone, PartialEq)]
enum Acting {
    /// `read`: render the window from the file's text.
    Read { window: super::tool::ReadWindow },
    /// `write`: the contents to write, and whether the target existed.
    Write { contents: String, existed: bool },
    /// `edit`: the replacement to apply to the text the read returns, then
    /// write the result.
    Edit { request: super::tool::EditRequest },
}

/// Where a call has got to.
#[derive(Debug, Clone, PartialEq)]
enum Stage {
    /// The approval waterfall is out; the chain's verdict decides.
    Asking {
        call: Call,
        args: Arguments,
        fence: Fence,
        /// The `approval/asked` id the chain used, stamped by the verdict.
        /// Carried so the emitter can write the audit pair with the *same* id
        /// the answerer recorded — the pair is unreadable if they differ.
        id: String,
    },
    /// A `Stat` is out, resolving and canonicalizing the target.
    Stat {
        call: Call,
        args: Arguments,
        fence: Fence,
        acting: Acting,
        path: String,
    },
    /// A read is out.
    Reading {
        call: Call,
        args: Arguments,
        fence: Fence,
        acting: Acting,
        canonical: String,
        /// The path as the model spelled it, for messages that name the
        /// target (`display_path` is applied on top of this).
        raw: String,
    },
    /// A write is out.
    Writing {
        call: Call,
        // The call's arguments are only needed by the tools that render from
        // them after the write; kept so a re-entry after the write needs nothing
        // that was not carried.
        _args: Arguments,
        fence: Fence,
        call_display: String,
        /// The write's own renderer inputs.
        render: Render,
    },
    /// A background call's detached start is out: the pid is what lands the
    /// job into the table, and the effect's answer carries it.
    StartingDetached {
        /// The command/description pair, for the job's label on the answer.
        request: super::tool_bash::BashRequest,
    },
    /// A detached read is out, draining the unread output for a `job_output`.
    ReadingJob {
        /// The job id the call is reading — the answer lands against it, so
        /// the table update and the outcome render must both name it.
        job: String,
    },
    /// A detached kill is out.
    KillingJob {
        job: String,
    },
    /// A confined command is out. Carries everything the result renderer needs,
    /// because classification and rendering both happen on the answer and
    /// nothing else survives the wait. The call itself is not carried: the
    /// queue still owns it, and `finish_call` reads it from there.
    Executing {
        request: super::tool_bash::BashRequest,
        mode: super::sandbox::Mode,
        enforcement: super::sandbox_runner::Enforcement,
        /// The runner's dialect, so the settled process can be classified as a
        /// denial or a runner failure rather than a plain nonzero exit.
        denial_signatures: Vec<&'static str>,
        runner_failure_rules: Vec<super::sandbox_runner::RunnerFailureRule>,
    },
}

/// What to render once a write lands.
#[derive(Debug, Clone, PartialEq)]
enum Render {
    Write {
        replace_all: bool,
        before: Option<String>,
    },
    Edit {
        replace_all: bool,
        before: String,
        after: String,
    },
}

/// What a `write`/`edit` is allowed to assume about its target: the
/// observation this session recorded for the path, from the `Stat` that
/// resolved it. Upstream mints the same pair in `fs-observation-policy`'s
/// `writeIntent` (`createIfAbsent` / `replaceIfVersion`).
#[derive(Debug, Clone, PartialEq)]
enum Observation {
    /// The path was read (or written) and was present at this version token.
    Present(String),
    /// The path was read and found absent, which authorizes a guarded create.
    Absent,
}

/// One session's job table: pid-keyed so a `ProcessRead`'s pid needs no
/// indirection, carrying the display identity the `job_list`/`job_output`
/// renderers report.
///
/// Upstream's shape is `jobs-local`'s per-owner registry; the table lives on
/// the executor's *owning machine* rather than in a service because the router
/// gives no machine-to-machine call — and the agent owns the catalog that
/// mints job ids, so it is the only owner the table can answer honestly for.
/// The model-facing id (`bash-N`, the registry's `<kind>-N` shape) is what the
/// tools name; the pid is what the driver names, and this table is the map
/// between them.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Jobs {
    /// Next sequence per session's own numbering: ids are predictable on
    /// purpose (`jobs-local`'s docs: "Ids are predictable, so authorization —
    /// not secrecy — is the boundary"), so the sequence needs no salt.
    next: u64,
    /// In start order, which is also registration order — what `job_list`
    /// reports.
    rows: Vec<JobRow>,
}

/// One job, as the table records it.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    /// The model-facing id, `bash-N` over the session's own sequence.
    pub name: String,
    /// The pid the driver answered at start, so the read/kill effects can be
    /// issued against it.
    pub pid: u32,
    /// The description the model passed, which upstream uses as the job's
    /// label — `job_list` renders it.
    pub label: String,
    /// The settled state once a read has observed it; `None` while running.
    /// Cached here rather than re-derived per call so a second `job_output`
    /// of a finished job answers the same settle, upstream's idempotent read.
    pub status: super::tool_bash::JobStatus,
}

impl Jobs {
    /// Start a job: assign its id, record it, and render the acknowledgment
    /// the model reads — upstream's `started background job <id>`.
    ///
    /// The pid comes from the driver's `ProcessStarted` answer, which is why
    /// this runs on the effect's answer rather than at the ask: the start is
    /// not recorded until the host has a handle.
    pub fn record(&mut self, pid: u32, label: String) -> String {
        self.next += 1;
        let name = format!("bash-{}", self.next);
        self.rows.push(JobRow {
            name: name.clone(),
            pid,
            label,
            status: super::tool_bash::JobStatus::Running,
        });
        name
    }

    /// Look a job up by the name the model knows it by.
    ///
    /// `None` is the refusal's raw fact; the wording is the tool's.
    pub fn find(&self, name: &str) -> Option<&JobRow> {
        self.rows.iter().find(|r| r.name == name)
    }

    /// Mark a job from a settle the read just observed.
    pub fn settle(&mut self, name: &str, status: super::tool_bash::JobStatus) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.name == name) {
            row.status = status;
        }
    }

    /// The list `job_list` renders, in registration order.
    pub fn list(&self) -> Vec<(String, super::tool_bash::JobStatus, String)> {
        self.rows
            .iter()
            .map(|r| (r.name.clone(), r.status.clone(), r.label.clone()))
            .collect()
    }
}

/// The executor's state for one step.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Executor {
    /// The turn and step every row this executor writes belongs to.
    ///
    /// Fixed for the executor's whole life: a step's calls are all made by one
    /// `assistant/message` at one position, so carrying the pair here is what
    /// lets the rows be built without asking the FSM on every append.
    position: (u64, u64),
    /// Calls still to run, in model order. The head is the one in flight.
    queue: Vec<Call>,
    stage: Option<Stage>,
    /// The `seq` of the in-flight call's `tool/call` row, which its result cites.
    call_seq: Option<u64>,
    /// Rows the executor owes the log, in order.
    owed: Vec<Value>,
    /// What this session has observed of each canonical path, keyed by the
    /// canonical path itself. The `Stat` that resolves a target records it
    /// (present-at-version or absent); a write or edit reads it back as the
    /// write's guard, exactly as upstream's `fs-observation-policy` records
    /// `fs/observed` and answers a stale or never-read target with
    /// `FS_STALE_VERSION` / `FS_NOT_OBSERVED`.
    observed: std::collections::BTreeMap<String, Observation>,
    /// The last outcome, so the agent can feed the FSM once the queue drains.
    done: bool,
}

impl Executor {
    /// A fresh executor for one step's calls.
    pub fn new(calls: Vec<Call>, turn: u64, step: u64) -> Self {
        Self {
            position: (turn, step),
            queue: calls,
            ..Default::default()
        }
    }

    /// Record the `seq` the log assigned to the row the executor just asked for.
    ///
    /// The agent calls this after appending each of the executor's rows, because
    /// only the agent knows the log's current length — `seq` belongs to the
    /// publisher, and a pure executor may not consult it. `row_type` is the
    /// provenance the executor already has, so it is matched here rather than
    /// re-derived from the row.
    pub fn observe_row(&mut self, row_type: &str, seq: u64) {
        if row_type == "tool/call" {
            self.call_seq = Some(seq);
        }
    }

    /// Start the next call: either an effect, an ask, or a refusal.
    ///
    /// `root` is the session's workspace root, which the fence needs and the
    /// machine resolves from the log's header row. `sandbox` carries the
    /// boot-resolved runner selection and command environment, because a
    /// confined command (`bash`) needs both and a machine may not resolve them.
    pub fn begin(
        &mut self,
        root: &str,
        fence: &Fence,
        sandbox: &super::tool_bash::SandboxContext,
        jobs: &Jobs,
    ) -> Vec<ExecutorOut> {
        if self.stage.is_some() {
            return Vec::new();
        }
        let Some(call) = self.queue.first().cloned() else {
            self.done = true;
            return Vec::new();
        };
        let args = Arguments::parse(&call.arguments);

        // A tool this host does not implement is an ordinary call failure. The
        // `tool/call` row is still written — the model made the call, and the
        // log is the record of what happened rather than of what succeeded.
        if !super::tool::catalog().iter().any(|t| t.name == call.name) {
            let mut outs = vec![self.call_row(&call)];
            outs.extend(self.finish_call(unknown_tool(&call.name)));
            return outs;
        }

        match gate(&call.name, &args, fence) {
            Gate::Refuse(outcome) => {
                let mut outs = vec![self.call_row(&call)];
                outs.extend(self.finish_call(outcome));
                outs
            }
            Gate::Allow => {
                let outs = self.start(&call, args, fence.clone(), root, sandbox, jobs);
                let mut rows = vec![self.call_row(&call)];
                rows.extend(outs);
                rows
            }
            Gate::Ask { reason } => {
                self.stage = Some(Stage::Asking {
                    call: call.clone(),
                    args,
                    fence: fence.clone(),
                    id: String::new(),
                });
                vec![
                    self.call_row(&call),
                    ExecutorOut::Dispatch {
                        name: "approval/request".into(),
                        payload: json!({
                            // The asker's identity, for the answerer chain. The
                            // `toolName`/`callId` pair is what a UI attaches the
                            // prompt to; `reason` is the audit text.
                            "toolName": call.name,
                            "callId": call.id,
                            "reason": reason,
                        }),
                        waterfall: true,
                    },
                ]
            }
        }
    }

    /// Answer the approval waterfall.
    ///
    /// The verdict is the chain's final value: `{outcome, approvalId}`. The id is
    /// how the emitter writes the audit pair with the same identity the answerer
    /// recorded — the approval machine owns the id because it owns the decision,
    /// and this machine owns the log, so the id has to travel between them.
    pub fn on_verdict(
        &mut self,
        verdict: &Value,
        root: &str,
        sandbox: &super::tool_bash::SandboxContext,
        jobs: &Jobs,
    ) -> Vec<ExecutorOut> {
        let Some(Stage::Asking {
            call, args, fence, ..
        }) = self.stage.take()
        else {
            return Vec::new();
        };
        let outcome = verdict
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("unavailable")
            .to_string();
        let id = verdict
            .get("approvalId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // The audit pair, written by the log's owner from the verdict. Both rows
        // are emitted before the result, which is the order the corpus shows.
        let mut outs = Vec::new();
        if !id.is_empty() {
            outs.push(ExecutorOut::Row {
                row_type: "approval/asked",
                data: json!({
                    "id": id,
                    "toolName": call.name,
                    "callId": call.id,
                    "reason": verdict.get("reason").cloned().unwrap_or(Value::Null),
                }),
            });
            outs.push(ExecutorOut::Row {
                row_type: "approval/decided",
                data: json!({ "id": id, "outcome": outcome }),
            });
        }

        if outcome == "allowed-once" {
            // A grant widens the fence for *this call only*, which is what makes
            // it one-shot.
            let requested = args
                .as_object()
                .and_then(|a| a.get("sandbox_permissions"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let widened = super::tool::escalated_fence(&fence, requested);
            outs.extend(self.start(&call, args, widened, root, sandbox, jobs));
        } else {
            // Bash words an escalation refusal its own way (`this command to
            // "<mode>"`); the filesystem family says `tool "<name>"`. The corpus
            // records both, so the mapping is per-family rather than shared.
            let requested = args
                .as_object()
                .and_then(|a| a.get("sandbox_permissions"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let outcome = if call.name == "bash" {
                super::tool_bash::escalation_denial(requested, &outcome)
            } else {
                super::tool::approval_denial(&call.name, &outcome)
            };
            outs.extend(self.finish_call(outcome));
        }
        outs
    }

    /// Answer an effect.
    pub fn on_effect(
        &mut self,
        effect: &Effect,
        answer: Result<Answer, String>,
        jobs: &mut Jobs,
    ) -> Vec<ExecutorOut> {
        let Some(stage) = self.stage.take() else {
            return Vec::new();
        };
        match (stage, effect) {
            // The target resolved: canonicalize, fence it, then act.
            (
                Stage::Stat {
                    call,
                    args,
                    fence,
                    acting,
                    path,
                },
                Effect::Stat { .. },
            ) => match answer {
                Ok(Answer::Stat {
                    canonical,
                    is_dir,
                    version,
                }) => {
                    if is_dir {
                        return self.finish_call(Outcome::denied(format!(
                            "cannot read \"{}\": not a regular file",
                            display_path(&fence.root, &path)
                        )));
                    }
                    if let Err(denial) = fence.check(std::path::Path::new(&canonical), &path) {
                        return self.finish_call(super::tool::confinement(&denial));
                    }
                    // The target exists, which is what decides create-vs-update
                    // *and* whether the write owes a diff. `start` cannot know
                    // it: existence is what the `Stat` was for.
                    let acting = match acting {
                        Acting::Write { contents, .. } => Acting::Write {
                            contents,
                            existed: true,
                        },
                        other => other,
                    };
                    // The observation gate runs *before* any read: an edit of a
                    // file this session never observed is refused here, which
                    // is upstream's `fs-observation-policy` `editIntent` — the
                    // read the pipeline is about to do is the edit's own
                    // left-hand side, not the authorizing observation. The
                    // version the map holds must first *agree* with the stat
                    // just taken: a stale entry is a refusal, not a basis.
                    if let Acting::Edit { .. } = &acting {
                        match self.observed.get(&canonical) {
                            Some(Observation::Present(v)) if Some(v) == version.as_ref() => {}
                            Some(_) => {
                                return self.finish_call(Outcome::denied(format!(
                                    "cannot edit \"{}\": file changed since it was read — re-read the file, then retry",
                                    display_path(&fence.root, &path)
                                )));
                            }
                            None => {
                                return self.finish_call(Outcome::denied(format!(
                                    "cannot modify \"{}\": file has not been read — read the file, then retry",
                                    display_path(&fence.root, &path)
                                )));
                            }
                        }
                    }
                    // A `write` over a file the session never observed is the
                    // same refusal in the policy's `writeIntent` words
                    // (`createIfAbsent` over an occupied target is
                    // `FS_NOT_OBSERVED`): the overwrite was never authorized.
                    // A stale observation is caught by the write's guard, not
                    // here — the write's intent is what the CAS compares.
                    if let Acting::Write { .. } = &acting
                        && !self.observed.contains_key(&canonical)
                    {
                        return self.finish_call(Outcome::denied(format!(
                            "cannot overwrite existing \"{}\": file has not been read — read the file, then retry",
                            display_path(&fence.root, &path)
                        )));
                    }
                    // The resolving `Stat` records the fresh observation — the
                    // pipeline's own stat is a world-read of the target, which
                    // is what `fs-local`'s stat-on-observe does.
                    self.observed.insert(
                        canonical.clone(),
                        match version {
                            Some(v) => Observation::Present(v),
                            None => Observation::Absent,
                        },
                    );
                    self.act(&call, args, fence, acting, canonical, path.clone())
                }
                // A missing target is not an error for `write`: it is a create.
                // It *is* one for `read` and `edit`, and each says so its own
                // way, so the decision belongs to the tool rather than here.
                Ok(Answer::NotFound) => match acting {
                    Acting::Write { contents, .. } => {
                        let canonical = creation_subject(&path);
                        if let Err(denial) = fence.check(std::path::Path::new(&canonical), &path) {
                            return self.finish_call(super::tool::confinement(&denial));
                        }
                        self.act(
                            &call,
                            args,
                            fence,
                            Acting::Write {
                                contents,
                                existed: false,
                            },
                            canonical,
                            path.clone(),
                        )
                    }
                    Acting::Read { .. } => self.finish_call(Outcome::denied(format!(
                        "cannot read \"{}\": not found",
                        display_path(&fence.root, &path)
                    ))),
                    Acting::Edit { .. } => self.finish_call(Outcome::denied(format!(
                        "cannot edit \"{}\": file changed since it was read",
                        display_path(&fence.root, &path)
                    ))),
                },
                Ok(Answer::Failed(message)) => self.finish_call(Outcome::denied(message)),
                _ => self.finish_call(Outcome::denied("the filesystem gave an unexpected answer")),
            },
            // The content arrived: render it, or apply the edit and write.
            (
                Stage::Reading {
                    call,
                    args,
                    fence,
                    acting,
                    canonical,
                    raw: _,
                },
                Effect::Read { .. },
            ) => {
                let text = match answer {
                    Ok(Answer::Text(t)) => t,
                    Ok(Answer::Failed(m)) => return self.finish_call(Outcome::denied(m)),
                    _ => return self.finish_call(Outcome::denied("unexpected read answer")),
                };
                match acting {
                    Acting::Read { window } => self.finish_call(read_outcome(&window, &text)),
                    Acting::Edit { request } => {
                        // The edit's guard is the recorded observation, taken
                        // before the text is touched: upstream checks the CAS
                        // basis *before* literal matching (`fs-local`'s
                        // `editText`), so a stale file reports "changed since
                        // it was read", not "old_string was not found".
                        let expect = match self.observed.get(&canonical) {
                            Some(Observation::Present(v)) => {
                                vocoder_cordis::WriteExpect::Version(v.clone())
                            }
                            // Unreachable: the Stat arm refuses an unobserved
                            // or absent target before this read is issued.
                            _ => vocoder_cordis::WriteExpect::Any,
                        };
                        match apply_edit(&text, &request) {
                        Ok(edited) => {
                            self.stage = Some(Stage::Writing {
                                call,
                                _args: args,
                                fence,
                                call_display: canonical.clone(),
                                render: Render::Edit {
                                    replace_all: request.replace_all,
                                    before: edited.before,
                                    after: edited.after.clone(),
                                },
                            });
                            vec![ExecutorOut::Effect(Effect::Write {
                                path: canonical,
                                contents: edited.after,
                                expect,
                            })]
                        }
                        Err(message) => self.finish_call(Outcome::denied(message)),
                        }
                    }
                    Acting::Write { contents, existed } => {
                        // The overwrite's observation was already required at
                        // the Stat arm (an unobserved existing target was
                        // refused there); the guard here carries the recorded
                        // version as the write's compare basis, so a file that
                        // changed between the stat and this write is refused
                        // "changed since it was read" rather than silently
                        // overwritten.
                        let expect = match self.observed.get(&canonical) {
                            Some(Observation::Present(v)) => {
                                vocoder_cordis::WriteExpect::Version(v.clone())
                            }
                            Some(Observation::Absent) => vocoder_cordis::WriteExpect::Absent,
                            None => vocoder_cordis::WriteExpect::Any,
                        };
                        self.stage = Some(Stage::Writing {
                            call,
                            _args: args,
                            fence,
                            call_display: canonical.clone(),
                            render: Render::Write {
                                replace_all: false,
                                before: existed.then_some(text),
                            },
                        });
                        vec![ExecutorOut::Effect(Effect::Write {
                            path: canonical,
                            contents,
                            expect,
                        })]
                    }
                }
            }
            // The write landed: render the confirmation.
            (
                Stage::Writing {
                    call,
                    render,
                    call_display,
                    ..
                },
                Effect::Write { .. },
            ) => match answer {
                Ok(Answer::Done) => {
                    let display = call_display;
                    match render {
                        Render::Write { before, .. } => {
                            let after = args_string(&call);
                            let op = if before.is_some() { "update" } else { "create" };
                            let diffs =
                                hunk_diffs(&display, before.as_deref().unwrap_or(""), &after);
                            self.finish_call(write_outcome(
                                &display,
                                op,
                                before.as_deref(),
                                &after,
                                diffs,
                            ))
                        }
                        Render::Edit {
                            replace_all,
                            before,
                            after,
                        } => {
                            let diffs = hunk_diffs(&display, &before, &after);
                            self.finish_call(edit_outcome(
                                &display,
                                replace_all,
                                &before,
                                &after,
                                diffs,
                            ))
                        }
                    }
                }
                Ok(Answer::Failed(m)) => self.finish_call(Outcome::denied(m)),
                _ => self.finish_call(Outcome::denied("the write did not land")),
            },
            // The detached start answered with the pid: the job lands in the
            // table under its minted id (the upstream registry's `<kind>-N`
            // shape, predictable on purpose — `jobs-local`'s docs note that
            // ids are predictable and the fence is authorization, not
            // secrecy), and the call closes on the acknowledgment upstream's
            // `render` produces for the `{kind: 'background'}` result.
            (
                Stage::StartingDetached { request },
                Effect::ExecDetached { .. },
            ) => {
                match answer {
                    Ok(Answer::ProcessStarted { pid }) => {
                        let id = jobs.record(pid, request.description.clone());
                        self.finish_call(Outcome::text(super::tool_bash::started_background(&id)))
                    }
                    // A spawn failure on the background path is the same
                    // runner-unusable class as a foreground one's: no job
                    // minted (upstream's contract — a throwing starter leaves
                    // nothing registered — so a failed start has no id to
                    // leak).
                    Ok(Answer::Failed(m)) => self.finish_call(Outcome::denied(m)),
                    _ => self.finish_call(Outcome::denied("the command gave an unexpected answer")),
                }
            }
            // The read drained: update the job's observed status, then render
            // the delta plus the status marker, in `tool-jobs`'s own shape.
            (
                Stage::ReadingJob { job },
                Effect::ExecRead { .. },
            ) => {
                match answer {
                    Ok(Answer::ProcessChunk {
                        running,
                        stdout_delta,
                        stderr_delta,
                        exit_code,
                        signal,
                        aborted,
                    }) => {
                        let status = if running {
                            super::tool_bash::JobStatus::Running
                        } else if aborted || signal.is_some() {
                            // `processOutcome`'s kill branch, verbatim: a kill
                            // stays killed, signaled when one is known.
                            super::tool_bash::JobStatus::Killed { signal }
                        } else {
                            super::tool_bash::JobStatus::Completed { exit_code }
                        };
                        jobs.settle(&job, status.clone());
                        let mut delta = stdout_delta;
                        if !stderr_delta.is_empty() {
                            if !delta.is_empty() && !delta.ends_with('\n') {
                                delta.push('\n');
                            }
                            delta.push_str("[stderr]\n");
                            delta.push_str(&stderr_delta);
                        }
                        self.finish_call(super::tool_bash::job_output_outcome(&delta, &status))
                    }
                    Ok(Answer::Failed(_m)) => {
                        // The driver answered "no such pid": the child is gone
                        // and its table entry was the name the machine knew it
                        // by, so refuse with the job-level wording a foreign
                        // id would have drawn at the gate.
                        self.finish_call(super::tool_bash::unknown_job(&job, false))
                    }
                    _ => self.finish_call(Outcome::denied("the job gave an unexpected answer")),
                }
            }
            // The kill's own ask is answered: the job still has to settle,
            // which upstream words as *requested* rather than *done* — the
            // `job_kill` render is `requested cancellation of job <id>`.
            (
                Stage::KillingJob { job },
                Effect::ExecKill { .. },
            ) => match answer {
                Ok(Answer::Done) => {
                    self.finish_call(super::tool_bash::job_kill_outcome(&job, None))
                }
                Ok(Answer::Failed(_m)) => {
                    self.finish_call(super::tool_bash::unknown_job(&job, false))
                }
                _ => self.finish_call(Outcome::denied("the job gave an unexpected answer")),
            },
            // The command settled: classify the sandbox, then render.
            (
                Stage::Executing {
                    request,
                    mode,
                    enforcement,
                    denial_signatures,
                    runner_failure_rules,
                },
                Effect::Exec { .. },
            ) => {
                let done = match answer {
                    Ok(Answer::Process {
                        exit_code,
                        signal,
                        stdout,
                        stderr,
                        truncated,
                        timed_out,
                        aborted,
                        spill_path,
                    }) => (exit_code, signal, stdout, stderr, truncated, timed_out, aborted, spill_path),
                    // A spawn failure is not a command that failed — no command
                    // ran. It is the runner being unusable, the same class as a
                    // missing backend, and it carries the same structured error.
                    Ok(Answer::Failed(m)) => {
                        return self.finish_call(
                            Outcome::denied(
                                super::sandbox_runner::Unavailable {
                                    mode,
                                    runner_detail: Some(m),
                                }
                                .message(),
                            )
                            .with_error(super::tool::sandbox_unavailable()),
                        );
                    }
                    _ => {
                        return self
                            .finish_call(Outcome::denied("the command gave an unexpected answer"));
                    }
                };
                let (exit_code, signal, stdout, stderr, truncated, timed_out, aborted, spill_path) =
                    done;
                // Runner failure outranks denial: if the sandbox never started,
                // nothing was denied, and the model must not read it as its
                // command's fault. Exit-gated and signature-matched, so a plain
                // nonzero exit is neither.
                if let Some(m) = super::sandbox_runner::classify_runner_failure(
                    exit_code,
                    &stderr,
                    &runner_failure_rules,
                ) {
                    return self.finish_call(
                        Outcome::denied(
                            super::sandbox_runner::Unavailable {
                                mode,
                                runner_detail: Some(m.detail),
                            }
                            .message(),
                        )
                        .with_error(super::tool::sandbox_unavailable()),
                    );
                }
                let denied = super::sandbox_runner::matches_signature(
                    exit_code,
                    &stderr,
                    &denial_signatures,
                );
                let run = super::tool_bash::BashRun {
                    exit_code,
                    signal,
                    timed_out,
                    aborted,
                    timeout_ms: request.timeout_ms,
                    stdout,
                    stderr,
                    truncated,
                    spill_path,
                    denied,
                    mode,
                    enforcement,
                };
                self.finish_call(super::tool_bash::bash_outcome(&run, true))
            }
            (stage, _) => {
                self.stage = Some(stage);
                Vec::new()
            }
        }
    }

    /// Resolve the target, then read or write as the tool needs.
    ///
    /// `bash` diverges here: it has no filesystem target to resolve, and its
    /// confinement is decided *once*, now, rather than after a `Stat`. That is
    /// what makes the fail-closed contract structural — there is no arm that
    /// hands a confined mode its original argv, and the unavailable case never
    /// reaches an effect at all.
    fn start(
        &mut self,
        call: &Call,
        args: Arguments,
        fence: Fence,
        root: &str,
        sandbox: &super::tool_bash::SandboxContext,
        jobs: &Jobs,
    ) -> Vec<ExecutorOut> {
        if call.name == "bash" {
            return self.start_bash(&args, &fence, root, sandbox);
        }
        // The three job controls take the same early return `bash` does and
        // for the same reason: no filesystem target, so no `Stat`. Their
        // "target" is the job table itself, and the only world they touch is
        // the detached process — reached through `ExecRead`/`ExecKill`, with
        // no confinement decision to make.
        if matches!(call.name.as_str(), "job_output" | "job_list" | "job_kill") {
            return self.start_job(call, &args, jobs);
        }
        let acting = match call.name.as_str() {
            "read" => match read_window(&args, root) {
                Ok(window) => Acting::Read { window },
                Err(message) => return self.finish_call(Outcome::denied(message)),
            },
            "write" => match write_request(&args, root) {
                Ok(req) => Acting::Write {
                    contents: req.contents,
                    existed: false,
                },
                Err(message) => return self.finish_call(Outcome::denied(message)),
            },
            "edit" => match edit_request(&args, root) {
                Ok(request) => Acting::Edit { request },
                Err(message) => return self.finish_call(Outcome::denied(message)),
            },
            other => return self.finish_call(unknown_tool(other)),
        };
        // Every tool resolves its target first: `read` to report absence and
        // refuse a directory, the mutators to fence the *canonical* path.
        let path = match &acting {
            Acting::Read { window } => window.path.clone(),
            Acting::Write { .. } => write_request(&args, root).unwrap().path,
            Acting::Edit { request } => request.path.clone(),
        };
        self.stage = Some(Stage::Stat {
            call: call.clone(),
            args,
            fence,
            acting,
            path: path.clone(),
        });
        vec![ExecutorOut::Effect(Effect::Stat { path })]
    }

    /// Confine a `bash` command and issue the exec effect, or fail closed.
    ///
    /// The confinement decision happens exactly once, here. Three outcomes:
    ///
    /// - A malformed call or a background request is a call failure (no effect).
    /// - A confining mode with no usable runner is a **fail-closed** refusal —
    ///   no effect is issued, so an unconfined command cannot run. The result
    ///   carries `SandboxUnavailableError`, the one bash failure with a
    ///   structured `data.error`.
    /// - Otherwise the argv is wrapped and the exec effect is issued; the runner
    ///   metadata travels with it so the settled process can be classified.
    fn start_bash(
        &mut self,
        args: &Arguments,
        fence: &Fence,
        root: &str,
        sandbox: &super::tool_bash::SandboxContext,
    ) -> Vec<ExecutorOut> {
        let request = match super::tool_bash::bash_request(args, root) {
            Ok(request) => request,
            Err(message) => return self.finish_call(Outcome::denied(message)),
        };
        // A model command is `bash -c <command>`, a fresh shell per call.
        let argv = vec![
            "bash".to_string(),
            "-c".to_string(),
            request.command.clone(),
        ];
        match super::sandbox_runner::confine(&argv, fence.mode(), root, &sandbox.selection) {
            Err(unavailable) => self.finish_call(
                Outcome::denied(unavailable.message())
                    .with_error(super::tool::sandbox_unavailable()),
            ),
            Ok(confined) => {
                // A background call neither blocks the step nor renders the
                // process's own output: the driver answers a pid, the machine
                // names the id, and the call closes on the acknowledgment —
                // which is what lets the queue advance while the command runs
                // (`tool-bash`'s `{kind: 'background'}` result, immediately).
                if request.background {
                    self.stage = Some(Stage::StartingDetached {
                        request: request.clone(),
                    });
                    return vec![ExecutorOut::Effect(Effect::ExecDetached {
                        confined,
                        workdir: request.workdir.clone(),
                        env: sandbox.env.clone(),
                    })];
                }
                let mode = fence.mode();
                let enforcement = confined.enforcement;
                let denial_signatures = confined.denial_signatures.clone();
                let runner_failure_rules = confined.runner_failure_rules.clone();
                self.stage = Some(Stage::Executing {
                    request: request.clone(),
                    mode,
                    enforcement,
                    denial_signatures,
                    runner_failure_rules,
                });
                vec![ExecutorOut::Effect(Effect::Exec {
                    confined,
                    mode,
                    workdir: request.workdir.clone(),
                    env: sandbox.env.clone(),
                    timeout_ms: request.timeout_ms,
                })]
            }
        }
    }

    /// A job control's start: resolve the id against the session's table and
    /// issue its effect, or refuse with the registry's own wording.
    ///
    /// Upstream's `tool-jobs` puts the ownership fence at the *service*
    /// (`jobs-local`'s `job.belongsTo` check); here the table the machine
    /// holds is already that session's own, so an unknown id and a foreign
    /// one are the same fact — this session owns no such job — and the
    /// refusal is the only answer either receives.
    fn start_job(&mut self, call: &Call, args: &Arguments, jobs: &Jobs) -> Vec<ExecutorOut> {
        use super::tool_bash::{JobStatus, job_kill_outcome, job_list_outcome, unknown_job};
        let parse_id = |args: &Arguments| -> Result<String, Outcome> {
            let Some(obj) = args.as_object() else {
                return Err(Outcome::denied("invalid arguments: expected an object"));
            };
            match obj.get("job_id").and_then(Value::as_str) {
                // `validateJobId`, verbatim.
                Some(id) if !id.is_empty() => Ok(id.to_string()),
                other => {
                    let got = serde_json::to_string(&other.unwrap_or("")).unwrap_or_default();
                    Err(Outcome::denied(format!(
                        "invalid job_id: expected a non-empty string, got {got}"
                    )))
                }
            }
        };
        match call.name.as_str() {
            // No effect at all: the table is the machine's own record, so a
            // list answers from it without touching the world. The settle
            // statuses are as observed at the last read; a job that has never
            // been read lists as running, matching upstream's snapshot shape.
            "job_list" => self.finish_call(job_list_outcome(&jobs.list())),
            "job_output" => {
                let id = match parse_id(args) {
                    Ok(id) => id,
                    Err(outcome) => return self.finish_call(outcome),
                };
                let Some(row) = jobs.find(&id) else {
                    return self.finish_call(unknown_job(&id, false));
                };
                // `wait: true` is the documented reduction: the driver's
                // synchronous effect loop has no wait to lean on (an effect
                // that blocks would hold the pump against the child it must
                // move), so the call answers immediately whether the job has
                // settled or not — upstream's timed-out wait returns
                // `[status: running]`, which is exactly what an immediate
                // read of a running job reports.
                let _ = args.as_object().and_then(|o| o.get("wait"));
                if let JobStatus::Running = row.status {
                    self.stage = Some(Stage::ReadingJob { job: id });
                    return vec![ExecutorOut::Effect(Effect::ExecRead { pid: row.pid })];
                }
                // Already observed as settled: upstream answers the drain
                // idempotently rather than re-killing the snapshot, so an
                // already-read job answers empty with its settled marker.
                self.finish_call(super::tool_bash::job_output_outcome("", &row.status))
            }
            "job_kill" => {
                let id = match parse_id(args) {
                    Ok(id) => id,
                    Err(outcome) => return self.finish_call(outcome),
                };
                let Some(row) = jobs.find(&id) else {
                    return self.finish_call(unknown_job(&id, false));
                };
                if let JobStatus::Running = row.status {
                    self.stage = Some(Stage::KillingJob { job: id });
                    return vec![ExecutorOut::Effect(Effect::ExecKill { pid: row.pid })];
                }
                // The kill arrived too late; the answer names the settle it
                // found, which is the `already-finished` render's branch.
                self.finish_call(job_kill_outcome(&id, Some(row.status.clone())))
            }
            other => self.finish_call(unknown_tool(other)),
        }
    }
    fn act(
        &mut self,
        call: &Call,
        args: Arguments,
        fence: Fence,
        acting: Acting,
        canonical: String,
        raw: String,
    ) -> Vec<ExecutorOut> {
        match &acting {
            // A read of the target, then the window is rendered from it. An edit
            // needs the same read: it is the left-hand side of the replacement.
            Acting::Read { .. } | Acting::Edit { .. } => {
                self.stage = Some(Stage::Reading {
                    call: call.clone(),
                    args,
                    fence,
                    acting,
                    canonical: canonical.clone(),
                    raw,
                });
                vec![ExecutorOut::Effect(Effect::Read { path: canonical })]
            }
            // A write over an existing file reads it first — not because the
            // write needs the text, but because the *diff* does, and a replay's
            // diff card is built from the persisted `meta`. A create has nothing
            // to read, so it writes straight through with `before: None`, which is
            // what makes its hunk a pure insertion.
            Acting::Write { contents, existed } => {
                if *existed {
                    // The unobserved-overwrite refusal fired at the Stat arm,
                    // so the map is known to hold this path by now.
                    self.stage = Some(Stage::Reading {
                        call: call.clone(),
                        args,
                        fence,
                        acting,
                        canonical: canonical.clone(),
                        raw,
                    });
                    return vec![ExecutorOut::Effect(Effect::Read { path: canonical })];
                }
                self.stage = Some(Stage::Writing {
                    call: call.clone(),
                    _args: args,
                    fence,
                    call_display: canonical.clone(),
                    render: Render::Write {
                        replace_all: false,
                        before: None,
                    },
                });
                // A create is guarded by the recorded absence; an unobserved
                // absent path is guarded the same way (`Absent` fails only if
                // the path turned out to exist, at which point the guard fires
                // — which is exactly `createIfAbsent`'s contract).
                vec![ExecutorOut::Effect(Effect::Write {
                    path: canonical,
                    contents: contents.clone(),
                    expect: vocoder_cordis::WriteExpect::Absent,
                })]
            }
        }
    }

    /// Finish the in-flight call with an outcome.
    fn finish_call(&mut self, outcome: Outcome) -> Vec<ExecutorOut> {
        let Some(call) = self.queue.first().cloned() else {
            return Vec::new();
        };
        let seq = self.call_seq.unwrap_or(0);
        self.queue.remove(0);
        self.stage = None;
        self.call_seq = None;
        self.owed.clear();
        let (turn, step) = self.position;
        vec![ExecutorOut::Row {
            row_type: "tool/result",
            data: outcome.row_data(turn, step, &call, seq),
        }]
    }

    /// The `tool/call` row for a call, at the executor's own position.
    fn call_row(&self, call: &Call) -> ExecutorOut {
        let (turn, step) = self.position;
        ExecutorOut::Row {
            row_type: "tool/call",
            data: call.row_data(turn, step),
        }
    }

    /// Whether every call has been run.
    pub fn finished(&self) -> bool {
        self.queue.is_empty() && self.stage.is_none()
    }
}

/// Split a decoded assistant message into its tool calls, in model order.
///
/// The message's `content` is the log's own shape (`{type: "tool-call", id,
/// name, arguments}`), so this reads the durable record rather than the stream
/// chunks: a call is whatever the settling message says it was.
///
/// `arguments` is a *string* on the wire and an object once `assemble_message`
/// has parsed the streamed fragments, so both are accepted and re-serialized to
/// text — one representation for the row and for the executor, which is the
/// model's own.
pub fn calls_in(message: &Value) -> Vec<Call> {
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    parts
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("tool-call"))
        .filter_map(|p| {
            let id = p.get("id").and_then(Value::as_str)?.to_string();
            Some(Call {
                id,
                name: p
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: match p.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                },
            })
        })
        .collect()
}

/// The `content` argument of a write, for its diff.
///
/// Read back from the raw arguments rather than carried: the render happens
/// after the write lands, and the arguments are the one thing already in hand.
fn args_string(call: &Call) -> String {
    Arguments::parse(&call.arguments)
        .as_object()
        .and_then(|a| a.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The fence's subject for a target that does not exist yet.
///
/// The path itself, and deliberately **not** re-derived against the workspace
/// root: an earlier version rejoined the root's components with the target's
/// suffix, which for an absolute path *outside* the root produced the root
/// itself — so a write to `/etc/x` was fenced as if it were the workspace, and
/// then failed with `Is a directory` rather than being denied. The path the
/// model named is the honest subject.
///
/// The narrowing this leaves: upstream resolves the deepest existing ancestor and
/// appends the rest, so a create under a *symlinked* directory lands where the
/// symlink points, while this judges the lexical path. A `..` is still resolved,
/// by [`super::sandbox::contains`] rather than by the kernel.
fn creation_subject(path: &str) -> String {
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machines::sandbox::Mode;

    fn call(name: &str, args: Value) -> Call {
        Call {
            id: format!("call_{name}"),
            name: name.into(),
            arguments: args.to_string(),
        }
    }

    fn fence() -> Fence {
        Fence::new(Mode::WorkspaceWrite, "/w")
    }

    /// The fail-closed sandbox a test that does not run `bash` uses.
    ///
    /// Every fs-tool test wants this: the executor resolves confinement only for
    /// `bash`, so an unavailable runner is inert — it refuses a command rather
    /// than changing how a `read` or `write` behaves.
    fn sandbox() -> super::super::tool_bash::SandboxContext {
        super::super::tool_bash::SandboxContext::default()
    }

    /// The effects an executor asked for, in order.
    fn effects(outs: &[ExecutorOut]) -> Vec<Effect> {
        outs.iter()
            .filter_map(|o| match o {
                ExecutorOut::Effect(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    /// The rows an executor produced, as `(type, data)`.
    fn rows(outs: &[ExecutorOut]) -> Vec<(&'static str, Value)> {
        outs.iter()
            .filter_map(|o| match o {
                ExecutorOut::Row { row_type, data } => Some((*row_type, data.clone())),
                _ => None,
            })
            .collect()
    }

    fn row_types(outs: &[ExecutorOut]) -> Vec<&'static str> {
        rows(outs).into_iter().map(|(t, _)| t).collect()
    }

    /// The `data` of the nth row.
    fn data(outs: &[ExecutorOut], n: usize) -> Value {
        rows(outs)[n].1.clone()
    }

    /// The model-facing text of the nth row's single content block.
    fn text_of(outs: &[ExecutorOut], n: usize) -> String {
        data(outs, n)["message"]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A read resolves, fences, reads, and renders — one call, three effects.
    #[test]
    fn a_read_resolves_then_reads() {
        let mut e = Executor::new(vec![call("read", json!({ "file_path": "a.txt" }))], 1, 1);
        let outs = e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        assert_eq!(row_types(&outs), vec!["tool/call"]);
        assert_eq!(
            effects(&outs),
            vec![Effect::Stat {
                path: "/w/a.txt".into()
            }]
        );
        e.observe_row("tool/call", 13);

        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
            version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        assert_eq!(
            effects(&outs),
            vec![Effect::Read {
                path: "/w/a.txt".into()
            }]
        );

        let outs = e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("hello\n".into())),
        &mut Jobs::default(),
        );
        let r: Vec<Value> = rows(&outs).into_iter().map(|(_, d)| d).collect();
        assert_eq!(r.len(), 1);
        // The result cites the call row's own seq, which is what pairs them.
        assert_eq!(r[0]["sourceEventSeqs"][0], 13);
        assert_eq!(r[0]["message"]["source"]["callId"], "call_read");
        let text = r[0]["message"]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("1: hello"), "{text}");
        assert!(e.finished());
    }

    /// A missing target for `read` is a refusal naming the path.
    #[test]
    fn a_missing_read_names_the_path() {
        let mut e = Executor::new(vec![call("read", json!({ "file_path": "gone.txt" }))], 1, 1);
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/gone.txt".into(),
            },
            Ok(Answer::NotFound),
        &mut Jobs::default(),
        );
        let r: Vec<Value> = rows(&outs).into_iter().map(|(_, d)| d).collect();
        assert_eq!(
            r[0]["message"]["content"][0]["content"][0]["text"],
            "Error: cannot read \"/w/gone.txt\": not found"
        );
        assert_eq!(r[0]["message"]["content"][0]["isError"], true);
    }

    /// A directory is refused by name, which is `resolveRegularReadTarget`'s
    /// second check.
    #[test]
    fn a_directory_is_not_a_regular_file() {
        let mut e = Executor::new(vec![call("read", json!({ "file_path": "d" }))], 1, 1);
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/d".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/d".into(),
                is_dir: true,
            version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        assert!(
            data(&outs, 0)["message"]["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("not a regular file")
        );
    }

    /// A write that finds the target absent is a create, and its diff is a pure
    /// insertion — `oldText` null, which is what a UI card keys off.
    #[test]
    fn a_write_creates_when_the_target_is_absent() {
        let mut e = Executor::new(
            vec![call(
                "write",
                json!({ "file_path": "new.txt", "content": "hi" }),
            )],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        assert_eq!(effects(&outs).len(), 1);
        e.observe_row("tool/call", 1);

        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/new.txt".into(),
            },
            Ok(Answer::NotFound),
        &mut Jobs::default(),
        );
        assert_eq!(
            effects(&outs),
            vec![Effect::Write {
                path: "/w/new.txt".into(),
                contents: "hi".into(),
            expect: vocoder_cordis::WriteExpect::Absent,
            }]
        );

        let outs = e.on_effect(
            &Effect::Write {
                path: "/w/new.txt".into(),
                contents: "hi".into(),
            expect: vocoder_cordis::WriteExpect::Absent,
            },
            Ok(Answer::Done),
        &mut Jobs::default(),
        );
        let r: Vec<Value> = rows(&outs).into_iter().map(|(_, d)| d).collect();
        assert!(
            r[0]["message"]["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Created file")
        );
        assert_eq!(r[0]["meta"]["diffs"][0]["oldText"], Value::Null);
        assert_eq!(r[0]["meta"]["diffs"][0]["newText"], "hi");
    }

    /// A write over an existing file reports `update` and diffs against what was
    /// there — which is why the target's text is read even though the write does
    /// not need it. The target is read *by the session* first, as upstream's
    /// `fs-write-overwrite` pins: the overwrite is authorized by that read.
    #[test]
    fn a_write_updates_when_the_target_exists() {
        let mut e = Executor::new(
            vec![
                call("read", json!({ "file_path": "a.txt" })),
                call("write", json!({ "file_path": "a.txt", "content": "new" })),
            ],
            1,
            1,
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("old\r\n".into())),
        &mut Jobs::default(),
        );
        // The write call now runs: its own stat, then the diff-read, then the
        // guarded write.
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 2);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        assert!(matches!(effects(&outs)[0], Effect::Read { .. }));
        let outs = e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("old\r\n".into())),
        &mut Jobs::default(),
        );
        assert!(matches!(effects(&outs)[0], Effect::Write { .. }));
        let outs = e.on_effect(
            &Effect::Write {
                path: "/w/a.txt".into(),
                contents: "new".into(),
                expect: vocoder_cordis::WriteExpect::Version("v1".into()),
            },
            Ok(Answer::Done),
        &mut Jobs::default(),
        );
        let r: Vec<Value> = rows(&outs).into_iter().map(|(_, d)| d).collect();
        assert!(
            r[0]["message"]["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Updated file")
        );
        assert_eq!(r[0]["meta"]["diffs"][0]["oldText"], "old");
    }

    /// An edit of a file the session first **read** — the compliant order the
    /// corpus pins (`fs-edit`): the read's `Stat` records the observation, and
    /// the edit then applies and writes with that version as its guard.
    #[test]
    fn an_edit_reads_applies_and_writes() {
        let mut e = Executor::new(
            vec![
                call("read", json!({ "file_path": "a.txt" })),
                call(
                    "edit",
                    json!({ "file_path": "a.txt", "old_string": "x", "new_string": "y" }),
                ),
            ],
            1,
            1,
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        assert!(matches!(effects(&outs)[0], Effect::Read { .. }));
        let _outs = e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("x y\n".into())),
        &mut Jobs::default(),
        );
        // The read is done; the edit is still queued, and running it left the
        // `read` call's result as the executor's owed row.
        assert!(!e.finished(), "the edit must still be queued");
        let outs = e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        assert!(!rows(&outs).is_empty(), "the read's result row is owed");
        e.observe_row("tool/call", 2);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        assert!(matches!(effects(&outs)[0], Effect::Read { .. }));

        let outs = e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("x y\n".into())),
        &mut Jobs::default(),
        );
        assert_eq!(
            effects(&outs),
            vec![Effect::Write {
                path: "/w/a.txt".into(),
                contents: "y y\n".into(),
                expect: vocoder_cordis::WriteExpect::Version("v1".into()),
            }]
        );
        let outs = e.on_effect(
            &Effect::Write {
                path: "/w/a.txt".into(),
                contents: "y y\n".into(),
                expect: vocoder_cordis::WriteExpect::Version("v1".into()),
            },
            Ok(Answer::Done),
        &mut Jobs::default(),
        );
        let r: Vec<Value> = rows(&outs).into_iter().map(|(_, d)| d).collect();
        assert_eq!(
            r[0]["message"]["content"][0]["content"][0]["text"],
            "The file /w/a.txt has been updated successfully."
        );
    }

    /// An edit of a file the session never read is refused before any
    /// filesystem effect beyond the resolving `Stat` — upstream's
    /// `fs-policy-reject` case, verbatim. The observation check fires *before*
    /// the read the pipeline would have issued, because that read is the
    /// edit's left-hand side, not an authorizing observation.
    #[test]
    fn an_edit_without_a_prior_read_is_refused() {
        let mut e = Executor::new(
            vec![call(
                "edit",
                json!({ "file_path": "settings.txt", "old_string": "a", "new_string": "b" }),
            )],
            1,
            1,
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/settings.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/settings.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        assert!(
            effects(&outs).is_empty(),
            "an unobserved edit must never reach a read or a write: {:?}",
            effects(&outs)
        );
        assert_eq!(
            data(&outs, 0)["message"]["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap(),
            "Error: cannot modify \"/w/settings.txt\": file has not been read — read the file, then retry"
        );
        assert!(e.finished());
    }

    /// An edit whose `old_string` is absent is refused without writing — after
    /// the observing read, so the policy is satisfied and `apply_edit` owns
    /// the refusal.
    #[test]
    fn an_edit_that_cannot_match_does_not_write() {
        let mut e = Executor::new(
            vec![
                call("read", json!({ "file_path": "a.txt" })),
                call(
                    "edit",
                    json!({ "file_path": "a.txt", "old_string": "zzz", "new_string": "y" }),
                ),
            ],
            1,
            1,
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("x y\n".into())),
        &mut Jobs::default(),
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 2);
        e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        let outs = e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("x y\n".into())),
        &mut Jobs::default(),
        );
        assert!(
            effects(&outs).is_empty(),
            "a refused edit must not reach the filesystem: {:?}",
            effects(&outs)
        );
        assert!(
            data(&outs, 0)["message"]["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("was not found")
        );
        assert!(e.finished());
    }

    /// The read-then-edit pipeline against a file that **changed** between the
    /// observation and the write: the write guard refuses with the stale
    /// wording, which is the CAS half of the policy the pipeline cannot see at
    /// the `Stat` (the edit's own re-read happened after the world moved).
    #[test]
    fn an_edit_on_a_stale_observation_is_refused_at_the_write() {
        let mut e = Executor::new(
            vec![
                call("read", json!({ "file_path": "a.txt" })),
                call(
                    "edit",
                    json!({ "file_path": "a.txt", "old_string": "x", "new_string": "y" }),
                ),
            ],
            1,
            1,
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("x y\n".into())),
        &mut Jobs::default(),
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 2);
        e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        let outs = e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("x y\n".into())),
        &mut Jobs::default(),
        );
        // The write was issued against v1; the answer says the world moved —
        // which is what `fs/stale-version` from the driver encodes.
        let write = effects(&outs);
        assert_eq!(write.len(), 1);
        let outs = e.on_effect(
            &write[0],
            Ok(Answer::Failed(
                "fs/stale-version: file changed since it was read\nrename /w/a.txt".into(),
            )),
        &mut Jobs::default(),
        );
        assert!(
            data(&outs, 0)["message"]["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("stale-version")
        );
        assert!(e.finished());
    }

    /// A `write` over a file the session never read is refused before any
    /// write is issued — upstream's `createIfAbsent`-over-occupied ⇒
    /// `FS_NOT_OBSERVED`, normalized to the not-observed wording.
    #[test]
    fn a_write_over_an_unread_file_is_refused() {
        let mut e = Executor::new(
            vec![call(
                "write",
                json!({ "file_path": "a.txt", "content": "new" }),
            )],
            1,
            1,
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        assert!(
            effects(&outs).is_empty(),
            "an unobserved overwrite must not reach the filesystem: {:?}",
            effects(&outs)
        );
        assert!(
            data(&outs, 0)["message"]["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("file has not been read — read the file, then retry")
        );
        assert!(e.finished());
    }

    /// A `write` over a file the session **read** proceeds, guarded by the
    /// observed version (the `fs-write-overwrite` order: read, then write).
    #[test]
    fn a_write_after_a_read_is_guarded_by_the_observed_version() {
        let mut e = Executor::new(
            vec![
                call("read", json!({ "file_path": "a.txt" })),
                call("write", json!({ "file_path": "a.txt", "content": "new" })),
            ],
            1,
            1,
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("old\n".into())),
        &mut Jobs::default(),
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 2);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
                version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        // The write's diff-read fires, then the write itself.
        assert!(matches!(effects(&outs)[0], Effect::Read { .. }));
        let outs = e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("old\n".into())),
        &mut Jobs::default(),
        );
        assert_eq!(
            effects(&outs),
            vec![Effect::Write {
                path: "/w/a.txt".into(),
                contents: "new".into(),
                expect: vocoder_cordis::WriteExpect::Version("v1".into()),
            }]
        );
    }

    /// The `fs-delete-recreate` shape: read (present), a shell `rm`, read
    /// (absent), then write — the create is guarded by the recorded absence.
    #[test]
    fn a_write_after_observing_absence_is_a_guarded_create() {
        let mut e = Executor::new(
            vec![
                call("read", json!({ "file_path": "a.txt" })),
                call("write", json!({ "file_path": "a.txt", "content": "back" })),
            ],
            1,
            1,
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        // The first read finds the file *absent* (the model already removed
        // it in the world, as `fs-delete-recreate` does through bash before
        // re-reading) — recording `Absent` is what authorizes the create.
        e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::NotFound),
        &mut Jobs::default(),
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 2);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::NotFound),
        &mut Jobs::default(),
        );
        assert_eq!(
            effects(&outs),
            vec![Effect::Write {
                path: "/w/a.txt".into(),
                contents: "back".into(),
                expect: vocoder_cordis::WriteExpect::Absent,
            }]
        );
    }

    /// The fence denies a mutation outside the workspace, and the denial names
    /// the mode — the shared marker a model recognizes from bash.
    #[test]
    fn a_confinement_denial_names_the_mode() {
        let mut e = Executor::new(
            vec![call(
                "write",
                json!({ "file_path": "/etc/passwd", "content": "x" }),
            )],
            1,
            1,
        );
        e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        let outs = e.on_effect(
            &Effect::Stat {
                path: "/etc/passwd".into(),
            },
            Ok(Answer::Stat {
                canonical: "/etc/passwd".into(),
                is_dir: false,
            version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        assert!(effects(&outs).is_empty(), "a denied write must not run");
        let text = data(&outs, 0)["message"]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            text.contains("[sandbox: file access denied under workspace-write mode]"),
            "{text}"
        );
        assert!(text.contains("/etc/passwd"), "{text}");
    }

    /// An unknown tool is a call failure, and it still gets a `tool/call` row:
    /// the model made the call, and the log records what happened.
    ///
    /// `subagent` stands in for the ~27 upstream tools this host deliberately
    /// does not compose — it is the honest example of an absent name now that
    /// `bash` is implemented.
    #[test]
    fn an_unknown_tool_is_a_result_not_a_turn_failure() {
        let mut e = Executor::new(vec![call("subagent", json!({ "command": "ls" }))], 1, 1);
        let outs = e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        assert_eq!(row_types(&outs), vec!["tool/call", "tool/result"]);
        assert!(effects(&outs).is_empty());
        assert_eq!(
            data(&outs, 1)["message"]["content"][0]["content"][0]["text"],
            "Error: unknown tool \"subagent\""
        );
        // The call's own arguments are preserved in the row, unparsed by this
        // host, because they are what the model actually sent.
        assert_eq!(data(&outs, 0)["arguments"], "{\"command\":\"ls\"}");
    }

    /// A plain mutation asks nothing — the corpus's own behaviour, and the
    /// correction this module's first draft needed.
    #[test]
    fn a_plain_mutation_dispatches_no_ask() {
        let mut e = Executor::new(
            vec![call(
                "write",
                json!({ "file_path": "a.txt", "content": "x" }),
            )],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, ExecutorOut::Dispatch { .. })),
            "{outs:?}"
        );
    }

    /// A widening request dispatches the ask, and a grant writes the audit pair
    /// *and then* runs the call — with the id the answerer used.
    #[test]
    fn a_granted_escalation_writes_the_audit_pair_then_runs() {
        let ro = Fence::new(Mode::ReadOnly, "/w");
        let mut e = Executor::new(
            vec![call(
                "write",
                json!({
                    "file_path": "a.txt",
                    "content": "x",
                    "sandbox_permissions": "workspace-write",
                    "justification": "the user asked",
                }),
            )],
            1,
            1,
        );
        let outs = e.begin("/w", &ro, &sandbox(), &Jobs::default());
        assert_eq!(row_types(&outs), vec!["tool/call"]);
        let dispatch = outs
            .iter()
            .find_map(|o| match o {
                ExecutorOut::Dispatch { name, payload, .. } => {
                    Some((name.clone(), payload.clone()))
                }
                _ => None,
            })
            .expect("an ask");
        assert_eq!(dispatch.0, "approval/request");
        assert_eq!(
            dispatch.1["reason"],
            "escalate sandbox to workspace-write: the user asked"
        );
        assert_eq!(dispatch.1["callId"], "call_write");

        let outs = e.on_verdict(
            &json!({ "outcome": "allowed-once", "approvalId": "approval-1" }),
            "/w",
            &sandbox(),
            &Jobs::default(),
        );
        assert_eq!(
            row_types(&outs),
            vec!["approval/asked", "approval/decided"],
            "the pair is written before the call runs"
        );
        let r: Vec<Value> = rows(&outs).into_iter().map(|(_, d)| d).collect();
        assert_eq!(r[0]["id"], "approval-1");
        assert_eq!(r[1]["id"], "approval-1");
        assert_eq!(r[1]["outcome"], "allowed-once");
        // The grant widened the fence, so the call proceeds.
        assert_eq!(effects(&outs).len(), 1);
    }

    /// A rejected ask writes the pair, runs nothing, and reports the refusal in
    /// the wording that tells the model a human said no.
    #[test]
    fn a_rejected_escalation_writes_the_pair_and_runs_nothing() {
        let ro = Fence::new(Mode::ReadOnly, "/w");
        let mut e = Executor::new(
            vec![call(
                "write",
                json!({
                    "file_path": "a.txt",
                    "content": "x",
                    "sandbox_permissions": "workspace-write",
                    "justification": "the user asked",
                }),
            )],
            1,
            1,
        );
        e.begin("/w", &ro, &sandbox(), &Jobs::default());
        let outs = e.on_verdict(
            &json!({ "outcome": "rejected", "approvalId": "approval-7" }),
            "/w",
            &sandbox(),
            &Jobs::default(),
        );
        assert_eq!(
            row_types(&outs),
            vec!["approval/asked", "approval/decided", "tool/result"]
        );
        assert!(effects(&outs).is_empty(), "a rejected call must not run");
        assert_eq!(
            data(&outs, 2)["message"]["content"][0]["content"][0]["text"],
            "Error: the user rejected tool \"write\""
        );
        assert_eq!(data(&outs, 1)["outcome"], "rejected");
        assert!(e.finished());
    }

    /// The fail-closed verdict denies, which is the whole point of the
    /// vocabulary: an unattended host refuses rather than silently permitting.
    ///
    /// The verdict here is what the *real* chain returns — the approval machine
    /// answers `unavailable` and still mints an id, because a fail-closed
    /// decision is a decision and the audit pair must record it.
    #[test]
    fn an_unavailable_verdict_fails_closed() {
        let ro = Fence::new(Mode::ReadOnly, "/w");
        let mut e = Executor::new(
            vec![call(
                "write",
                json!({
                    "file_path": "a.txt",
                    "content": "x",
                    "sandbox_permissions": "workspace-write",
                    "justification": "the user asked",
                }),
            )],
            1,
            1,
        );
        e.begin("/w", &ro, &sandbox(), &Jobs::default());
        let outs = e.on_verdict(
            &json!({
                "outcome": "unavailable",
                "approvalId": "approval-1",
                "reason": "escalate sandbox to workspace-write: the user asked",
            }),
            "/w",
            &sandbox(),
            &Jobs::default(),
        );
        assert!(effects(&outs).is_empty(), "a denied call must not run");
        assert_eq!(
            row_types(&outs),
            vec!["approval/asked", "approval/decided", "tool/result"],
            "a denial is still an audited decision"
        );
        let r: Vec<Value> = rows(&outs).into_iter().map(|(_, d)| d).collect();
        assert_eq!(r[0]["id"], "approval-1");
        assert_eq!(r[1]["outcome"], "unavailable");
        let text = r[2]["message"]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("no approval channel is available"), "{text}");
        assert!(e.finished());
    }

    /// A verdict that names no id writes no audit pair.
    ///
    /// This is the `unavailable`-without-an-asker case spelled out: the pair is
    /// only writable when a real ask produced an id, and inventing one here would
    /// record a question no answerer ever saw.
    #[test]
    fn a_verdict_without_an_id_writes_no_pair() {
        let ro = Fence::new(Mode::ReadOnly, "/w");
        let mut e = Executor::new(
            vec![call(
                "write",
                json!({
                    "file_path": "a.txt",
                    "content": "x",
                    "sandbox_permissions": "workspace-write",
                    "justification": "the user asked",
                }),
            )],
            1,
            1,
        );
        e.begin("/w", &ro, &sandbox(), &Jobs::default());
        let outs = e.on_verdict(
            &json!({ "outcome": "unavailable" }),
            "/w",
            &sandbox(),
            &Jobs::default(),
        );
        assert_eq!(row_types(&outs), vec!["tool/result"]);
    }

    /// A malformed escalation is refused at the gate, so no question is put to
    /// anyone and the call never runs.
    #[test]
    fn a_malformed_escalation_never_reaches_the_chain() {
        let mut e = Executor::new(
            vec![call(
                "write",
                json!({ "file_path": "a.txt", "sandbox_permissions": "danger-full-access" }),
            )],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        assert_eq!(row_types(&outs), vec!["tool/call", "tool/result"]);
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, ExecutorOut::Dispatch { .. }))
        );
        assert!(effects(&outs).is_empty());
        assert!(
            data(&outs, 1)["message"]["content"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("requires a justification")
        );
    }

    /// Several calls run in model order, one at a time, each with its own rows.
    #[test]
    fn calls_run_in_model_order_one_at_a_time() {
        let mut e = Executor::new(
            vec![
                call("read", json!({ "file_path": "a.txt" })),
                call("read", json!({ "file_path": "b.txt" })),
            ],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        e.observe_row("tool/call", 1);
        assert_eq!(
            effects(&outs)[0],
            Effect::Stat {
                path: "/w/a.txt".into()
            }
        );

        e.on_effect(
            &Effect::Stat {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Stat {
                canonical: "/w/a.txt".into(),
                is_dir: false,
            version: Some("v1".into()),
            }),
        &mut Jobs::default(),
        );
        let outs = e.on_effect(
            &Effect::Read {
                path: "/w/a.txt".into(),
            },
            Ok(Answer::Text("a\n".into())),
        &mut Jobs::default(),
        );
        assert!(!e.finished(), "the second call is still queued");
        // The next call does not begin until the agent asks for it, which is
        // what keeps a call's rows contiguous.
        assert!(effects(&outs).is_empty());

        let outs = e.begin("/w", &fence(), &sandbox(), &Jobs::default());
        assert_eq!(row_types(&outs), vec!["tool/call"]);
        assert_eq!(
            effects(&outs)[0],
            Effect::Stat {
                path: "/w/b.txt".into()
            }
        );
    }

    // -----------------------------------------------------------------------
    // bash: the host's first tool that executes code, and the one that makes
    // the kernel sandbox load-bearing. These drive the seam end to end without
    // spawning: the executor decides confinement, asks for the exec effect, and
    // classifies the settled process the answer describes.
    // -----------------------------------------------------------------------

    /// A sandbox context whose runner really confines (`bwrap`, full).
    fn confining() -> super::super::tool_bash::SandboxContext {
        use super::super::sandbox_runner::{Enforcement, Runner, Selection};
        super::super::tool_bash::SandboxContext {
            selection: Selection::Confined(Runner::Bwrap, Enforcement::Full),
            env: vec![("PATH".into(), "/usr/bin".into())],
        }
    }

    fn bash_call(args: Value) -> Call {
        call("bash", args)
    }

    /// A well-formed command is confined and issues exactly one exec effect,
    /// carrying the wrapped argv and the runner's own metadata.
    #[test]
    fn a_command_is_confined_then_run() {
        let mut e = Executor::new(
            vec![bash_call(
                json!({ "command": "echo hi", "description": "Say hi" }),
            )],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &confining(), &Jobs::default());
        assert_eq!(row_types(&outs), vec!["tool/call"]);
        let Effect::Exec {
            confined,
            mode,
            workdir,
            env,
            timeout_ms,
        } = &effects(&outs)[0]
        else {
            panic!("expected an exec effect, got {:?}", effects(&outs));
        };
        // The argv is `bwrap <profile> -- bash -c <command>`: already wrapped, so
        // the driver spawns it without deciding anything.
        assert_eq!(confined.argv[0], "bwrap");
        assert_eq!(
            &confined.argv[confined.argv.len() - 3..],
            &["bash", "-c", "echo hi"]
        );
        assert_eq!(*mode, Mode::WorkspaceWrite);
        assert_eq!(workdir, "/w");
        assert_eq!(env, &vec![("PATH".to_string(), "/usr/bin".to_string())]);
        assert_eq!(
            *timeout_ms,
            super::super::tool_bash::BASH_DEFAULT_TIMEOUT_MS
        );
        assert!(!e.finished(), "the command is still out");
    }

    /// A plain exit renders the body and no marker — the corpus's own shape.
    #[test]
    fn a_settled_command_renders_its_output() {
        let mut e = Executor::new(
            vec![bash_call(
                json!({ "command": "echo hi", "description": "Say hi" }),
            )],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &confining(), &Jobs::default());
        let effect = effects(&outs)[0].clone();
        let outs = e.on_effect(
            &effect,
            Ok(Answer::Process {
                exit_code: Some(0),
                signal: None,
                stdout: "hi\n".into(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                aborted: false,
                spill_path: None,
            }),
        &mut Jobs::default(),
        );
        assert_eq!(row_types(&outs), vec!["tool/result"]);
        let text = text_of(&outs, 0);
        assert_eq!(text, "hi\n");
        // A nonzero exit is reported, not errored; so is a denial.
        assert!(e.finished());
    }

    /// A denial is classified from the runner's dialect: `bwrap`'s kernel says
    /// `Read-only file system`, and the marker plus the escalation hint follow.
    #[test]
    fn a_denied_command_is_classified_from_the_dialect() {
        let mut e = Executor::new(
            vec![bash_call(
                json!({ "command": "touch /etc/x", "description": "Write outside" }),
            )],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &confining(), &Jobs::default());
        let effect = effects(&outs)[0].clone();
        let outs = e.on_effect(
            &effect,
            Ok(Answer::Process {
                exit_code: Some(1),
                signal: None,
                stdout: String::new(),
                stderr: "touch: cannot touch '/etc/x': Read-only file system\n".into(),
                truncated: false,
                timed_out: false,
                aborted: false,
                spill_path: None,
            }),
        &mut Jobs::default(),
        );
        let text = text_of(&outs, 0);
        assert!(
            text.contains("[sandbox: file access denied under workspace-write mode]"),
            "{text}"
        );
        assert!(text.contains("retry this exact command"), "{text}");
        // A denial is a result the model reacts to, not an error.
        assert!(data(&outs, 0)["message"]["content"][0]["isError"] != json!(true));
    }

    /// A runner that fails before executing its profile is *not* a denial: the
    /// reserved exit plus the runner's own signature classify it as
    /// unavailable, and it carries the one structured bash error.
    #[test]
    fn a_runner_failure_outranks_a_denial() {
        let mut e = Executor::new(
            vec![bash_call(
                json!({ "command": "true", "description": "No-op" }),
            )],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &confining(), &Jobs::default());
        let effect = effects(&outs)[0].clone();
        let outs = e.on_effect(
            &effect,
            Ok(Answer::Process {
                exit_code: Some(1),
                signal: None,
                stdout: String::new(),
                stderr: "bwrap: Creating new namespace failed\n".into(),
                truncated: false,
                timed_out: false,
                aborted: false,
                spill_path: None,
            }),
        &mut Jobs::default(),
        );
        let text = text_of(&outs, 0);
        assert!(text.contains("no sandbox backend is usable"), "{text}");
        // The one bash failure with a structured error, because it is an
        // infrastructure failure rather than a result.
        assert_eq!(data(&outs, 0)["error"]["name"], "SandboxUnavailableError");
    }

    /// A confining mode with no usable runner refuses *without* issuing an
    /// effect — that is what makes the fail-closed contract structural rather
    /// than a discipline: there is no arm that hands a confined mode its argv.
    #[test]
    fn an_unavailable_runner_refuses_before_any_effect() {
        let mut e = Executor::new(
            vec![bash_call(json!({ "command": "ls", "description": "List" }))],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &Jobs::default()); // fail-closed default
        assert_eq!(row_types(&outs), vec!["tool/call", "tool/result"]);
        assert!(effects(&outs).is_empty(), "no command runs unconfined");
        let text = text_of(&outs, 1);
        assert!(text.contains("no sandbox backend is usable"), "{text}");
        assert_eq!(data(&outs, 1)["error"]["name"], "SandboxUnavailableError");
        assert!(e.finished());
    }

    /// `danger-full-access` is the mode whose meaning is "do not confine", so it
    /// runs unwrapped — and that is deliberate, not the forbidden passthrough.
    #[test]
    fn danger_full_access_runs_the_argv_unwrapped() {
        let mut e = Executor::new(
            vec![bash_call(json!({ "command": "ls", "description": "List" }))],
            1,
            1,
        );
        let ro = Fence::new(Mode::DangerFullAccess, "/w");
        let outs = e.begin("/w", &ro, &sandbox(), &Jobs::default());
        let Effect::Exec { confined, .. } = &effects(&outs)[0] else {
            panic!("expected an exec effect");
        };
        assert_eq!(confined.argv, vec!["bash", "-c", "ls"]);
    }

    /// A malformed call is refused before confinement is even considered.
    #[test]
    fn a_malformed_command_is_refused() {
        let mut e = Executor::new(vec![bash_call(json!({ "command": "ls" }))], 1, 1);
        // Missing `description`.
        let outs = e.begin("/w", &fence(), &confining(), &Jobs::default());
        assert!(effects(&outs).is_empty());
        let text = text_of(&outs, 1);
        assert!(text.contains("invalid description"), "{text}");
    }

    // -----------------------------------------------------------------------
    // Background jobs: the `run_in_background` arm and the three `job_*`
    // controls, driven purely — the executor names the effects and updates the
    // table; the driver's own tests cover the real spawn.
    // -----------------------------------------------------------------------

    /// A background call mints the job on the start's answer and closes
    /// immediately — never blocking the step.
    #[test]
    fn a_background_call_detaches_and_reports_the_id() {
        let mut e = Executor::new(
            vec![bash_call(
                json!({ "command": "sleep 10", "description": "Sleep ten seconds", "run_in_background": true }),
            )],
            1,
            1,
        );
        let mut jobs = Jobs::default();
        let outs = e.begin("/w", &fence(), &confining(), &jobs);
        assert_eq!(row_types(&outs), vec!["tool/call"]);
        // One effect, the detached one — the blocked executor never starts.
        let Effect::ExecDetached { confined, .. } = &effects(&outs)[0] else {
            panic!("expected a detached exec, got {:?}", effects(&outs));
        };
        assert_eq!(confined.argv[0], "bwrap");
        assert!(!e.finished(), "the call is still out, waiting only on the pid");

        let outs = e.on_effect(
            &effects::last_detached(&outs),
            Ok(Answer::ProcessStarted { pid: 4242 }),
            &mut jobs,
        );
        assert_eq!(row_types(&outs), vec!["tool/result"]);
        // Upstream's `render` over `{kind: 'background'}`, verbatim.
        assert_eq!(text_of(&outs, 0), "started background job bash-1");
        // The table recorded the pid against the label the model passed.
        let listed = jobs.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "bash-1");
        assert_eq!(listed[0].2, "Sleep ten seconds");
        assert!(e.finished());
    }

    /// A `job_output` on a running job drains the delta and reports the
    /// `[status: running]` marker upstream's `statusLine` renders.
    #[test]
    fn a_job_output_reports_running_then_completed() {
        let mut jobs = Jobs::default();
        jobs.record(4242, "Sleep ten seconds".into());
        let mut e = Executor::new(
            vec![call("job_output", json!({ "job_id": "bash-1" }))],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        let Effect::ExecRead { pid } = &effects(&outs)[0] else {
            panic!("expected a read effect, got {:?}", effects(&outs));
        };
        assert_eq!(*pid, 4242);

        let outs = e.on_effect(
            &Effect::ExecRead { pid: 4242 },
            Ok(Answer::ProcessChunk {
                running: true,
                stdout_delta: "one\n".into(),
                stderr_delta: String::new(),
                exit_code: None,
                signal: None,
                aborted: false,
            }),
            &mut jobs,
        );
        let text = text_of(&outs, 0);
        assert_eq!(text, "one\n[status: running]");

        // The settle arrives on a later read: `completed` with the exit code
        // as detail, which is `processOutcome`'s own spelling.
        let mut e = Executor::new(
            vec![call("job_output", json!({ "job_id": "bash-1" }))],
            1,
            2,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        let outs = e.on_effect(
            &effects(&outs)[0],
            Ok(Answer::ProcessChunk {
                running: false,
                stdout_delta: "two\n".into(),
                stderr_delta: String::new(),
                exit_code: Some(0),
                signal: None,
                aborted: false,
            }),
            &mut jobs,
        );
        let text = text_of(&outs, 0);
        assert_eq!(text, "two\n[status: completed, exit code: 0]");
        // The observed settle sticks: a third read is idempotent, on the
        // table rather than the driver.
        let mut e = Executor::new(
            vec![call("job_output", json!({ "job_id": "bash-1" }))],
            1,
            3,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        assert!(effects(&outs).is_empty(), "a settled job answers from the table");
        assert_eq!(text_of(&outs, 1), "(no new output)\n[status: completed, exit code: 0]");
    }

    /// A `job_output`'s `[stderr]` section joins the delta the way bash's own
    /// body does, and a signal death reports `killed` with its signal.
    #[test]
    fn a_job_output_reports_a_kill_with_its_signal() {
        let mut jobs = Jobs::default();
        jobs.record(4242, "run server".into());
        let mut e = Executor::new(
            vec![call("job_output", json!({ "job_id": "bash-1" }))],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        let outs = e.on_effect(
            &effects(&outs)[0],
            Ok(Answer::ProcessChunk {
                running: false,
                stdout_delta: "ready\n".into(),
                stderr_delta: "killed\n".into(),
                exit_code: None,
                signal: Some(15),
                aborted: true,
            }),
            &mut jobs,
        );
        let text = text_of(&outs, 0);
        assert_eq!(
            text,
            "ready\n[stderr]\nkilled\n[status: killed, signal: SIGTERM]"
        );
    }

    /// `job_kill` asks for the kill and answers *requested*, never *done* —
    /// the job still has to settle, which a later `job_output` observes.
    #[test]
    fn a_job_kill_requests_cancellation() {
        let mut jobs = Jobs::default();
        jobs.record(4242, "run server".into());
        let mut e = Executor::new(
            vec![call("job_kill", json!({ "job_id": "bash-1" }))],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        let Effect::ExecKill { pid } = &effects(&outs)[0] else {
            panic!("expected a kill effect, got {:?}", effects(&outs));
        };
        assert_eq!(*pid, 4242);
        let outs = e.on_effect(
            &Effect::ExecKill { pid: 4242 },
            Ok(Answer::Done),
            &mut jobs,
        );
        assert_eq!(text_of(&outs, 0), "requested cancellation of job bash-1");
    }

    /// A kill reaching an already-settled job names the settle it found.
    #[test]
    fn a_kill_of_a_settled_job_reports_already_finished() {
        let mut jobs = Jobs::default();
        jobs.record(4242, "run server".into());
        jobs.settle(
            "bash-1",
            super::super::tool_bash::JobStatus::Completed { exit_code: Some(0) },
        );
        let mut e = Executor::new(
            vec![call("job_kill", json!({ "job_id": "bash-1" }))],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        assert!(effects(&outs).is_empty(), "no effect: the job is already over");
        assert_eq!(
            text_of(&outs, 1),
            "job bash-1 had already finished [status: completed, exit code: 0]"
        );
    }

    /// An unknown id is refused with the registry's own wording, on every
    /// control — and never issues an effect.
    #[test]
    fn an_unknown_job_is_refused_by_name() {
        let jobs = Jobs::default();
        for (tool, args) in [
            ("job_output", json!({ "job_id": "bash-99" })),
            ("job_kill", json!({ "job_id": "bash-99" })),
        ] {
            let mut e = Executor::new(vec![call(tool, args)], 1, 1);
            let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
            assert!(
                effects(&outs).is_empty(),
                "{tool}: an unknown id must not reach the driver"
            );
            assert_eq!(text_of(&outs, 1), "Error: unknown job bash-99");
        }
        let mut e = Executor::new(vec![call("job_output", json!({}))], 1, 1);
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        assert!(
            text_of(&outs, 1).contains("invalid job_id"),
            "{}",
            text_of(&outs, 1)
        );
    }

    /// `job_list` renders registration order, and the empty table's own line.
    #[test]
    fn a_job_list_reports_every_job_in_order() {
        let jobs = Jobs::default();
        let mut e = Executor::new(vec![call("job_list", json!({}))], 1, 1);
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        assert!(effects(&outs).is_empty());
        assert_eq!(text_of(&outs, 1), "(no background jobs)");

        let mut jobs = Jobs::default();
        jobs.record(1, "sleep one".into());
        jobs.settle(
            "bash-1",
            super::super::tool_bash::JobStatus::Killed { signal: Some(15) },
        );
        jobs.record(2, "sleep two".into());
        let mut e = Executor::new(vec![call("job_list", json!({}))], 1, 1);
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        assert_eq!(
            text_of(&outs, 1),
            "bash-1 [bash] killed — sleep one\nbash-2 [bash] running — sleep two"
        );
    }

    /// The catalog's absence check for the tool surface this module reduced:
    /// `wait: true` parses and still answers from a read rather than blocking.
    #[test]
    fn a_wait_read_is_the_reduction_answering_immediately() {
        let mut jobs = Jobs::default();
        jobs.record(4242, "sleep".into());
        let mut e = Executor::new(
            vec![call(
                "job_output",
                json!({ "job_id": "bash-1", "wait": true, "timeout_ms": 5000 }),
            )],
            1,
            1,
        );
        let outs = e.begin("/w", &fence(), &sandbox(), &jobs);
        // One non-blocking read, rather than a suspend the pump cannot serve.
        assert!(matches!(effects(&outs)[0], Effect::ExecRead { pid: 4242 }));
    }

    /// The foreground path is untouched by the background arm: a plain call
    /// still issues the blocking exec and renders as it always has.
    #[test]
    fn a_foreground_call_is_unchanged() {
        let mut e = Executor::new(
            vec![bash_call(json!({ "command": "echo hi", "description": "Say hi" }))],
            1,
            1,
        );
        let mut jobs = Jobs::default();
        let outs = e.begin("/w", &fence(), &confining(), &jobs);
        assert!(matches!(effects(&outs)[0], Effect::Exec { .. }));
        let outs = e.on_effect(
            &effects(&outs)[0],
            Ok(Answer::Process {
                exit_code: Some(0),
                signal: None,
                stdout: "hi\n".into(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                aborted: false,
                spill_path: None,
            }),
            &mut jobs,
        );
        assert_eq!(text_of(&outs, 0), "hi\n");
        assert!(jobs.list().is_empty(), "no job minted for a foreground call");
    }
}

/// Small helpers the job tests share; kept out of the executor's own path so
/// the tests read as sequences of calls rather than plumbing.
#[cfg(test)]
mod effects {
    use super::*;

    /// The last (and only) detached effect a batch of outs asked for.
    pub fn last_detached(outs: &[ExecutorOut]) -> Effect {
        outs.iter()
            .filter_map(|o| match o {
                ExecutorOut::Effect(e) => Some(e.clone()),
                _ => None,
            })
            .last()
            .expect("an effect")
    }
}
