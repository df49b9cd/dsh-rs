//! The tool seam's argument parsing and model-facing rendering.
//!
//! This module is deliberately **pure and I/O-free**: it takes a call's
//! arguments and the bytes the world returned, and produces the exact text the
//! model reads. Everything that touches the world lives in [`super::tool_exec`].
//!
//! ## Why the rendered text is a contract
//!
//! `tool-fs`'s model-facing strings are what the recorded snapshot corpus holds,
//! so a replay compares against them byte for byte. The `<path>…</path>` /
//! `<type>file</type>` / `<content>…</content>` envelope, the `1: line`
//! numbering, the `(End of file - total N lines)` footer, the
//! `... (line truncated to N chars)` suffix, `Created file` / `Updated file`, and
//! the two edit sentences are all reproduced verbatim from
//! `tool-fs/src/read-render.ts`, `write.ts` and `edit.ts`. Tests quote the
//! upstream source's own strings so a drift in either one fails here.
//!
//! ## The three reductions from upstream, each stated rather than implied
//!
//! - **No streaming reads.** Upstream streams a file at or above
//!   `readStreamMinSize` (10 MiB) and buffers below it. This host always buffers,
//!   and enforces the window and byte caps while scanning, so peak memory is the
//!   file size rather than unbounded — the same guarantee for every file a
//!   model can realistically ask for, without a second effect path.
//! - **The version guard is real, not implied.** Upstream's
//!   `fs-observation-policy` requires a prior read of the file in this session
//!   and pins a version CAS basis. The executor reproduces it end to end —
//!   observation is recorded on every resolving `Stat`, the write carries the
//!   observed version into `RealizeRequest::WriteText.expect`, and the driver
//!   re-stats under the write before the rename — with the policy's own
//!   refusals (`cannot modify "…": file has not been read`, `cannot edit "…":
//!   file changed since it was read`).
//! - **No `\r\n` normalization.** Upstream's wider fs layer normalizes the diff
//!   basis. This host strips a trailing `\r` per line for display, exactly as
//!   `buildWindow` does, and leaves the bytes how it found them.

// The module is complete and tested but **not yet wired**: the executor that
// drives it is the next step, so in a non-test build every item here reads as
// dead code. The allow is scoped to the module rather than sprinkled per item so
// that removing it later is one deletion, and so the reason is stated once.
// `machines/agent_loop.rs` carries the same allow for the same reason.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::sandbox::{Denial, Escape, Fence, Mode, widening};

/// The `readLimit` config default (`tool-fs/src/read.ts`).
pub const READ_LIMIT: u64 = 2000;
/// The `readMaxLineLength` config default (`tool-fs/src/read-render.ts`).
pub const READ_MAX_LINE_LENGTH: usize = 2000;
/// The `readMaxBytes` config default (`tool-fs/src/read-render.ts`).
pub const READ_MAX_BYTES: usize = 50 * 1024;

/// One model-requested call.
///
/// The arguments are the model's raw JSON *text*, because that is what the log
/// records: `tool/call`'s `arguments` is a string, and upstream's
/// `parseArguments` preserves unparseable input as text rather than failing the
/// call. Parsing before the row would quietly discard what the model said.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl Call {
    /// The `tool/call` row's `data`, under an open turn and step.
    ///
    /// `callId` rather than `id`: the row's own identity is its `seq`, and the
    /// model's id for the call is what the result has to cite. `data` rather than
    /// the whole row because the row type is the executor's to name.
    pub fn row_data(&self, turn: u64, step: u64) -> Value {
        json!({
            "turn": turn,
            "step": step,
            "callId": self.id,
            "name": self.name,
            "arguments": self.arguments,
        })
    }
}

/// A call's arguments after parsing.
///
/// Unparseable input is preserved as text, mirroring upstream's
/// `parseArguments`: a call whose arguments did not parse still *happened*, and
/// recording it with an empty object would be a durable lie about the request.
#[derive(Debug, Clone, PartialEq)]
pub enum Arguments {
    /// A JSON object.
    Object(serde_json::Map<String, Value>),
    /// Well-formed JSON that is not an object, or malformed input, as text.
    NotAnObject(String),
}

impl Arguments {
    pub fn parse(raw: &str) -> Self {
        if raw.trim().is_empty() {
            // Upstream maps empty input to `{}`.
            return Self::Object(serde_json::Map::new());
        }
        match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(map)) => Self::Object(map),
            // A non-object is rendered back as text rather than as its JSON
            // form, so a number or a string reads as what the model typed.
            Ok(v) => Self::NotAnObject(v.to_string()),
            Err(_) => Self::NotAnObject(raw.to_string()),
        }
    }

    pub fn as_object(&self) -> Option<&serde_json::Map<String, Value>> {
        match self {
            Self::Object(map) => Some(map),
            Self::NotAnObject(_) => None,
        }
    }
}

/// One tool this host offers, as the model sees it.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// The JSON Schema for the arguments — the wire wants a schema, so a schema
    /// is what is stored, not upstream's `parameters` DSL.
    pub input_schema: Value,
}

/// The tools offered to the model.
///
/// **Closed on purpose.** Upstream's base profile composes around thirty tools
/// across bash, subagents, skills, jobs, search and the cordis tooling; offering
/// a name this host cannot execute would produce a result reading
/// `unknown tool`, which teaches the model the tool exists and is broken. A
/// shorter honest list is the only correct choice until the rest exist.
///
/// The descriptions and schemas are `tool-fs`'s, verbatim, including the two
/// escalation fields on the mutators.
///
/// **The escalation fields are advertised, and their absence was a defect.** An
/// earlier version omitted `sandbox_permissions` and `justification` on the
/// reasoning that this host "offers no wider mode to escalate to". That was
/// wrong: `ESCALATION_TARGETS` is a closed vocabulary of modes a call may
/// escalate *to*, [`Mode::targets`] carries it, and the executor's gate already
/// asks on a strictly-wider request. Omitting the fields did not remove
/// escalation — it removed the *discovery* of it, leaving a model that hit a
/// denial with no sanctioned move except to fail. `tool-fs` spreads
/// `schemaFields()` into exactly the two mutators whenever a confining backend is
/// mounted, and this host's backend confines, so the fields belong here.
pub fn catalog() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read",
            description: "Read a UTF-8 text file and return line-numbered content.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "file_path": { "type": "string", "description": "Path to read, resolved by the filesystem backend." },
                    "offset": { "type": "number", "description": "1-based first line to return. Defaults to 1." },
                    "limit": { "type": "number", "description": format!("Maximum number of lines to return. Defaults to {READ_LIMIT}.") },
                },
                "required": ["file_path"],
            }),
        },
        ToolSpec {
            name: "write",
            description: "Create or fully replace a UTF-8 text file.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "file_path": { "type": "string", "description": "Path to write, resolved by the filesystem backend." },
                    "content": { "type": "string", "description": "Full UTF-8 text content to write." },
                    "sandbox_permissions": {
                        "type": "string",
                        "enum": escalation_targets(),
                        "description": ESCALATION_PERMISSIONS_DESCRIPTION,
                    },
                    "justification": {
                        "type": "string",
                        "description": ESCALATION_JUSTIFICATION_DESCRIPTION,
                    },
                },
                "required": ["file_path", "content"],
            }),
        },
        ToolSpec {
            name: "edit",
            description: "Edit an existing UTF-8 text file by replacing literal text.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "file_path": { "type": "string", "description": "Path to edit, resolved by the filesystem backend." },
                    "old_string": { "type": "string", "description": "Literal text to replace. Must match exactly." },
                    "new_string": { "type": "string", "description": "Literal replacement text. Use an empty string to delete the match." },
                    "replace_all": { "type": "boolean", "description": "Replace all matches. Defaults to false; when false, old_string must appear exactly once." },
                    "sandbox_permissions": {
                        "type": "string",
                        "enum": escalation_targets(),
                        "description": ESCALATION_PERMISSIONS_DESCRIPTION,
                    },
                    "justification": {
                        "type": "string",
                        "description": ESCALATION_JUSTIFICATION_DESCRIPTION,
                    },
                },
                "required": ["file_path", "old_string", "new_string"],
            }),
        },
        ToolSpec {
            name: "bash",
            description: BASH_DESCRIPTION,
            input_schema: json!({
                "type": "object",
                // Unlike the fs tools, this schema does NOT set
                // `additionalProperties: false`: upstream's own leaves extra
                // keys alone, so the value rules the schema cannot express
                // (the non-empty `command`/`description`, the positive
                // `timeoutMs`) are the executor's to enforce rather than the
                // validator's.
                "properties": {
                    "command": { "type": "string", "description": "The bash command to execute." },
                    "description": {
                        "type": "string",
                        "description": "Clear, concise description of what this command does in active voice, 5-10 words (shown in the UI). Examples: \"ls\" -> \"List files in current directory\"; \"git status\" -> \"Show working tree status\"; \"npm install\" -> \"Install package dependencies\".",
                    },
                    "timeoutMs": { "type": "number", "description": "Timeout in milliseconds. The executor applies its configured default and cap, and kills the command on expiry." },
                    "workdir": { "type": "string", "description": "Working directory for this command. Defaults to the session workspace; a relative path is resolved against it." },
                    // Verbatim from `tool-bash`'s `schemaFields` at
                    // `enableRunInBackground: true`: this host now composes a
                    // jobs registry, so the field is offered and honored.
                    "run_in_background": { "type": "boolean", "description": "Run in the background and return a job id immediately (collect with job_output, stop with job_kill). No timeout applies." },
                    "sandbox_permissions": {
                        "type": "string",
                        "enum": escalation_targets(),
                        "description": ESCALATION_PERMISSIONS_DESCRIPTION,
                    },
                    "justification": {
                        "type": "string",
                        "description": ESCALATION_JUSTIFICATION_DESCRIPTION,
                    },
                },
                "required": ["command", "description"],
            }),
        },
        // `tool-jobs`'s three controls, verbatim schemas and descriptions. The
        // jobs registry exists (`AgentMachine::jobs`), so these are honest
        // entries rather than taught-then-broken names.
        ToolSpec {
            name: "job_output",
            description: "Read a background job. Stream jobs return only output since the previous read; final-output jobs return their result after settlement. Every response ends with `[status: ...]`. Reads are non-blocking unless `wait: true`, which waits up to the configured cap.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "job_id": { "type": "string", "description": "Job id returned by the tool that started the background work." },
                    "wait": { "type": "boolean", "description": "Block until the job reaches a terminal status or the timeout expires. A timed-out wait returns [status: running] and leaves the job alive." },
                    "timeout_ms": { "type": "number", "description": "Max wait in milliseconds (only meaningful with wait: true). Defaults to the configured wait timeout; capped by the configured maximum." },
                },
                "required": ["job_id"],
            }),
        },
        ToolSpec {
            name: "job_list",
            description: "List your background jobs (running and finished) with their ids, kinds, and statuses.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {},
            }),
        },
        ToolSpec {
            name: "job_kill",
            description: "Request cancellation of a running background job by job id. Returns immediately; the job settles as killed once its work actually stops.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "job_id": { "type": "string", "description": "Job id returned by the tool that started the background work." },
                    "reason": { "type": "string", "description": "Optional short reason, recorded in the log and forwarded to the job." },
                },
                "required": ["job_id"],
            }),
        },
    ]
}

/// The `bash` tool's description, upstream's `bashDescription` verbatim at
/// `enableRunInBackground: true` with escalation advertised.
///
/// Copied rather than paraphrased because it is prompt text a model acts on: it
/// teaches the fresh-shell contract, the exit marker, the sandbox denial marker,
/// the `run_in_background` → `job_output`/`job_kill` pairing, and — load-bearing
/// for the escalation ladder — the sanctioned one-shot retry. A paraphrase would
/// change what the model does on a denial without changing anything a test could
/// see, which is exactly the kind of drift the corpus exists to prevent.
///
/// The enabled-background sentence ("Set `run_in_background: true`…") and the
/// escalation paragraph are both upstream's own `bashDescription` branches — this
/// host composes the `enableRunInBackground: true` variant with
/// `escalationModes.length > 0`.
pub const BASH_DESCRIPTION: &str = concat!(
    "Execute a bash command (`bash -c`) and return its stdout/stderr. ",
    "Each call runs in a fresh shell: no state (cwd, variables, functions) persists between calls — ",
    "pass `workdir` instead of using `cd`. Non-zero exits are reported as `[exit code: N]`. ",
    "Current harness environment facts are exposed through managed `$DSH_*` variables; inspect them when needed. ",
    "Commands may run under a file sandbox; a blocked file operation is reported as ",
    "`[sandbox: file access denied under <mode> mode]` — a policy denial, not a bug in the command; do not retry another way. ",
    "Long output is truncated to its tail; the full output is saved to a file whose path is reported when available. ",
    "Set `run_in_background: true` for long-running commands: the call returns a job id immediately; read its output with `job_output` and stop it with `job_kill`. ",
    "Attempting a command the sandbox may deny is safe and expected: run it and read the ",
    "marker rather than assuming the denial. When a command is denied and a wider mode would let it ",
    "succeed, escalate immediately in the same turn — the one sanctioned exception to a denial: retry ",
    "the exact same command once with `sandbox_permissions` (the narrowest wider mode that suffices) ",
    "plus a one-sentence `justification`. Do not detour through chat to ask permission first — the ",
    "approval prompt raised by that retry is how the user consents. If the session states approval ",
    "prompts are disabled, there is no exception: a denial is final — do not set `sandbox_permissions`. ",
    "Never escalate speculatively: ground the request in a real denial — normally the one this command ",
    "just hit; escalating up front is fine only when this session already denied the same access. ",
    "A rejected escalation is final for that command — stop and explain, never work around ",
    "it — but it does not forbid attempting or escalating other commands later.",
);

/// The escalation target names, as the schema's `enum` carries them.
///
/// `read-only` is absent because it is the floor: nothing escalates *to* it.
/// The list is [`Mode::targets`]' spelling, kept in one place so the vocabulary a
/// model is offered and the vocabulary the gate validates cannot drift.
pub fn escalation_targets() -> Vec<&'static str> {
    Mode::targets().iter().map(|m| m.as_str()).collect()
}

/// `sandbox_permissions`' description, verbatim from `schemaFields()`.
///
/// The clause "Only valid as a one-shot retry of an operation the sandbox just
/// denied" is doing real work: without it a model may read the field as a
/// standing grant and set it prophylactically, which turns a narrow escalation
/// into a blanket one.
pub const ESCALATION_PERMISSIONS_DESCRIPTION: &str = "The wider sandbox mode this file operation needs. Only valid as a one-shot retry \
     of an operation the sandbox just denied; requires justification and user approval.";

/// `justification`'s description, verbatim.
pub const ESCALATION_JUSTIFICATION_DESCRIPTION: &str = "Required with sandbox_permissions: one sentence for the user explaining \
     why this exact file operation needs the wider access.";

/// The same-turn escalation hint that rides a denial.
///
/// `escalationHintMarker`, verbatim. It lives *at the decision point* on purpose,
/// so the sanctioned retry does not depend on the model recalling the tool
/// description — which is why it is emitted on the denial rather than only
/// advertised in the schema.
///
/// `subject` is the family's noun: `operation` for a filesystem mutation,
/// `command` for bash. The two differ because a model retrying the wrong kind of
/// thing is a live failure mode.
pub fn escalation_hint(subject: &str) -> String {
    format!(
        "[sandbox: escalation available — retry this exact {subject} once with \
         sandbox_permissions (the narrowest wider mode that suffices) + justification; \
         the approval prompt asks the user]"
    )
}

/// The canonical [`ToolSpec`] list as `llm_dialect` tools.
///
/// The translation core renders these per dialect, so the model gets a real
/// `tools` array rather than a prompt sentence describing one.
pub fn tool_definitions() -> Vec<llm_dialect::items::Tool> {
    catalog()
        .into_iter()
        .map(|t| llm_dialect::items::Tool {
            name: t.name.to_string(),
            description: Some(t.description.to_string()),
            input_schema: t.input_schema,
        })
        .collect()
}

/// Whether a tool mutates the workspace, and so must clear the approval gate.
///
/// `read` is deliberately absent: reading already-confined content is not what an
/// approval prompt exists to guard, and asking about every read makes the prompt
/// meaningless noise. Upstream reaches the same split through
/// `tools/pre-execute`, whose only registrants in the base profile are the
/// escalation paths on the mutating tools.
pub fn is_mutating(name: &str) -> bool {
    matches!(name, "write" | "edit")
}

/// The outcome of one call, ready to become a `tool/result` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// The model-facing content blocks.
    pub content: Vec<Value>,
    pub is_error: bool,
    /// The tool's private presentation payload, persisted so a UI card survives
    /// replay (`meta` in the row).
    pub meta: Option<Value>,
    /// The failure's structured info, for `data.error`.
    ///
    /// **`data.error`, and only there.** The corpus is unambiguous: of 107
    /// `tool/result` rows, five carry an `error` key and every one of them has it
    /// on `data` — never on the message part — and every one is shaped
    /// `{ name, code }`. An earlier version of this struct put a `{ message }`
    /// object on the *part* as well, which was wrong twice over: in the wrong
    /// place, and in a vocabulary no reader of the log knows. The text the model
    /// sees is already in the content block; `error` is the structured
    /// classification a *program* reads, and none of the five recorded names
    /// (`FsError`, `SandboxUnavailableError`, `WebError`) is a message.
    pub error: Option<Value>,
}

/// A structured failure classification, as `data.error` carries it.
///
/// Two fields and no third: the corpus's five examples are all
/// `{name, code}` and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorInfo {
    /// The error class, e.g. `FsError`, `SandboxUnavailableError`.
    pub name: String,
    /// The machine-readable code, e.g. `FS_NOT_FOUND`, `SANDBOX_UNAVAILABLE`.
    pub code: String,
}

impl ErrorInfo {
    pub fn new(name: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            code: code.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "name": self.name, "code": self.code })
    }
}

/// The fail-closed sandbox error's class, as the corpus records it.
///
/// A function rather than a `const`: `ErrorInfo` owns `String`s, and a `const`
/// cannot hold one. Returning a fresh value also keeps it impossible for a
/// caller to mutate a shared classification.
pub fn sandbox_unavailable() -> ErrorInfo {
    ErrorInfo::new("SandboxUnavailableError", "SANDBOX_UNAVAILABLE")
}

/// The filesystem error class, as the corpus records it.
pub fn fs_error(code: &str) -> ErrorInfo {
    ErrorInfo::new("FsError", code)
}

impl Outcome {
    /// A successful result with one text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![json!({ "type": "text", "text": text.into() })],
            is_error: false,
            meta: None,
            error: None,
        }
    }

    /// A failure, rendered as `Error: <text>`.
    ///
    /// The prefix is upstream's, applied where a denial or a thrown error is
    /// materialized into content — it is what lets the model tell a refusal from
    /// a tool's own output.
    ///
    /// No structured `error` by default: a denial that is the *tool's own*
    /// judgment (an unknown tool name, a bad argument) has no upstream error
    /// class, and inventing one would put a name in the log that no reader
    /// recognizes. Callers that do have a class attach it with
    /// [`Self::with_error`].
    pub fn denied(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            content: vec![json!({ "type": "text", "text": format!("Error: {text}") })],
            is_error: true,
            meta: None,
            error: None,
        }
    }

    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = Some(meta);
        self
    }

    /// Attach the structured classification `data.error` carries.
    pub fn with_error(mut self, error: ErrorInfo) -> Self {
        self.error = Some(error.to_json());
        self
    }

    /// The `tool/result` row for this outcome.
    ///
    /// The message is **user-role** — that is what upstream's
    /// `createToolResultMessage` produces, and it is why a tool result travels
    /// back to the model as an input item rather than as assistant text.
    /// `sourceEventSeqs` cites the `tool/call` row, which is what lets a reader
    /// pair them after the fact; `surfaceOp: "append"` is the surface bookkeeping
    /// every corpus tool/result carries.
    pub fn row_data(&self, turn: u64, step: u64, call: &Call, call_seq: u64) -> Value {
        let part = json!({
            "type": "tool-result",
            "toolCallId": call.id,
            "content": self.content,
            "isError": self.is_error,
        });
        let mut data = json!({
            "turn": turn,
            "step": step,
            "message": {
                "source": { "kind": "tool", "callId": call.id },
                "content": [part],
                "role": "user",
            },
            "sourceEventSeqs": [call_seq],
            "surfaceOp": "append",
        });
        if let Some(meta) = &self.meta {
            data["meta"] = meta.clone();
        }
        // Last, and on `data`: the classification is a sibling of the message,
        // not a field of it.
        if let Some(error) = &self.error {
            data["error"] = error.clone();
        }
        data
    }
}

/// The result a call naming a tool this host does not implement produces.
///
/// The text is upstream's, so a model that learned the vocabulary elsewhere
/// recognizes it. A missing tool is an ordinary call failure, not a turn
/// failure: a model that invented a name should be told so and continue.
pub fn unknown_tool(name: &str) -> Outcome {
    Outcome::denied(format!("unknown tool \"{name}\""))
}

/// The denial a fail-closed approval verdict produces.
///
/// The three non-granting outcomes get *distinct* text, because the model's next
/// move differs: a human "no" should be respected, a cancellation may be worth
/// retrying, and an absent approval channel means asking again is pointless.
/// Upstream's `serviceAsk` words them this way.
pub fn approval_denial(tool_name: &str, outcome: &str) -> Outcome {
    match outcome {
        "rejected" => Outcome::denied(format!("the user rejected tool \"{tool_name}\"")),
        "cancelled" => Outcome::denied(format!("approval for tool \"{tool_name}\" was cancelled")),
        _ => Outcome::denied(format!(
            "tool \"{tool_name}\" requires approval, but no approval channel is available"
        )),
    }
}

/// The model-facing path: absolute stays as given, relative resolves against the
/// session's workspace root.
///
/// Upstream's backend resolves the path and reports a `displayPath`; this host
/// has no separate resolved form, so the join *is* the display path. An absolute
/// path is left alone rather than joined, which is what the corpus shows for a
/// read outside the workspace.
pub fn display_path(root: &str, raw: &str) -> String {
    if Path::new(raw).is_absolute() {
        return raw.to_string();
    }
    PathBuf::from(root).join(raw).to_string_lossy().to_string()
}

/// A positive-integer argument, or the default when absent.
fn positive(args: &serde_json::Map<String, Value>, key: &str, default: u64) -> Result<u64, String> {
    let Some(v) = args.get(key) else {
        return Ok(default);
    };
    match v.as_u64() {
        Some(n) if n >= 1 => Ok(n),
        _ => Err(format!("{key} must be a positive integer")),
    }
}

/// The `read` window: which lines to return, and what to say when asked for
/// lines that are not there.
///
/// Separated from rendering because the two are different questions and only one
/// of them needs the file's bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadWindow {
    pub path: String,
    pub offset: u64,
    pub limit: u64,
}

/// Parse `read`'s arguments, applying upstream's defaults and caps.
pub fn read_window(arguments: &Arguments, root: &str) -> Result<ReadWindow, String> {
    let Some(args) = arguments.as_object() else {
        return Err("invalid arguments: expected an object".into());
    };
    let Some(raw) = args.get("file_path").and_then(Value::as_str) else {
        return Err("file_path must be a non-empty string".into());
    };
    if raw.trim().is_empty() {
        return Err("file_path must be a non-empty string".into());
    }
    let offset = positive(args, "offset", 1)?;
    let limit = positive(args, "limit", READ_LIMIT)?;
    if limit > READ_LIMIT {
        return Err(format!("limit must be less than or equal to {READ_LIMIT}"));
    }
    Ok(ReadWindow {
        path: display_path(root, raw),
        offset,
        limit,
    })
}

/// Render a `read` result from the file's decoded text.
///
/// Faithful to `buildWindow` + `formatReadOutput`, including the two details a
/// port is most likely to lose: an `offset` past EOF is `FS_NOT_FOUND` rather
/// than an empty window (except for an empty file at offset 1), and the byte cap
/// stops the scan while the *total* line count is still exact.
pub fn read_outcome(window: &ReadWindow, text: &str) -> Outcome {
    let lines: Vec<&str> = split_lines(text);
    let total = lines.len();

    if window.offset > total as u64 && !(total == 0 && window.offset == 1) {
        return Outcome::denied(format!(
            "offset {} is out of range for \"{}\" ({} lines)",
            window.offset, window.path, total
        ));
    }

    let mut kept: Vec<Value> = Vec::new();
    let mut bytes = 0usize;
    let mut capped = false;
    for (i, line) in lines.iter().enumerate() {
        let number = i as u64 + 1;
        if number < window.offset || kept.len() as u64 >= window.limit {
            continue;
        }
        let rendered = truncate_line(line);
        // The separator counts toward the cap, matching `lineByteSize`'s
        // `+ (currentLineCount > 0 ? 1 : 0)`.
        let size = rendered.len() + usize::from(!kept.is_empty());
        if bytes + size > READ_MAX_BYTES {
            capped = true;
            break;
        }
        bytes += size;
        kept.push(json!({ "number": number, "text": rendered }));
    }

    let end_line = kept
        .last()
        .and_then(|l| l.get("number"))
        .and_then(Value::as_u64)
        .unwrap_or(window.offset.saturating_sub(1));
    let footer = if capped {
        format!(
            "(Output capped. Showing lines {}-{end_line}. Use offset={} to continue.)",
            window.offset,
            end_line + 1
        )
    } else if end_line < total as u64 {
        format!(
            "(Showing lines {}-{end_line} of {total}. Use offset={} to continue.)",
            window.offset,
            end_line + 1
        )
    } else {
        format!("(End of file - total {total} lines)")
    };
    let body = if kept.is_empty() {
        footer
    } else {
        let numbered = kept
            .iter()
            .map(|l| {
                format!(
                    "{}: {}",
                    l.get("number").and_then(Value::as_u64).unwrap_or(0),
                    l.get("text").and_then(Value::as_str).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("{numbered}\n\n{footer}")
    };
    let rendered = format!(
        "<path>{}</path>\n<type>file</type>\n<content>\n{body}\n</content>",
        window.path
    );

    let mut meta = json!({
        "path": window.path,
        "offset": window.offset,
        "lines": kept,
        "totalLines": total,
    });
    if let Some(lang) = lang_from_path(&window.path) {
        meta["lang"] = json!(lang);
    }
    Outcome::text(rendered).with_meta(meta)
}

/// Split text into lines the way `buildWindow` does: on `\n`, a trailing `\r`
/// stripped, and a final unterminated line still counted.
///
/// Two properties are load-bearing and each was wrong in a first attempt:
///
/// - The `\r` is stripped **per line, after the split**. Stripping it from the
///   whole text first — the obvious reading of "remove the carriage returns" —
///   turns `a\r\nb` into `a\nb`, one line fewer than the file has, so every line
///   number after the first `\r\n` would be off by one.
/// - **Empty text is zero lines, not one.** `"".split('\n')` yields `[""]`, but
///   `buildWindow` flushes a line only when it has bytes, so an empty file is
///   zero lines and renders `(End of file - total 0 lines)`. Treating it as one
///   empty line would also make a pure insertion's diff carry an `oldText` of
///   `""` where upstream reports `null`.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').map(strip_cr).collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// One line without its trailing carriage return.
fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// The per-line character cap and its suffix.
fn truncate_line(line: &str) -> String {
    if line.chars().count() > READ_MAX_LINE_LENGTH {
        let cut: String = line.chars().take(READ_MAX_LINE_LENGTH).collect();
        format!("{cut}... (line truncated to {READ_MAX_LINE_LENGTH} chars)")
    } else {
        line.to_string()
    }
}

/// The syntax-highlighting hint `langFromPath` derives from an extension.
///
/// A port of `read-render.ts`'s `LANG_BY_EXTENSION`, narrow on purpose: common
/// source, config and markup extensions, not an exhaustive registry. Two details
/// are load-bearing and easy to lose in a port — a leading dot is a dotfile with
/// *no* extension (`.gitignore`), and the lookup must not match a filename whose
/// extension is a prototype key (`foo.constructor` maps to nothing).
pub fn lang_from_path(path: &str) -> Option<&'static str> {
    let base = path.rsplit_once(['/', '\\']).map_or(path, |(_, b)| b);
    let dot = base.rfind('.')?;
    if dot == 0 {
        return None;
    }
    Some(match base[dot + 1..].to_lowercase().as_str() {
        "ts" | "mts" | "cts" => "ts",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "js",
        "jsx" => "jsx",
        "json" | "jsonc" => "json",
        "py" => "py",
        "rb" => "rb",
        "go" => "go",
        "rs" => "rs",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" | "cxx" => "cpp",
        "cs" => "cs",
        "kt" => "kotlin",
        "swift" => "swift",
        "php" => "php",
        "sh" | "bash" | "zsh" => "sh",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "ini" => "ini",
        "md" | "markdown" => "md",
        "mdx" => "mdx",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "sql" => "sql",
        "xml" => "xml",
        "lua" => "lua",
        _ => return None,
    })
}

/// A `write` request after argument validation.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteRequest {
    pub path: String,
    pub contents: String,
}

/// Parse `write`'s arguments.
pub fn write_request(arguments: &Arguments, root: &str) -> Result<WriteRequest, String> {
    let Some(args) = arguments.as_object() else {
        return Err("invalid arguments: expected an object".into());
    };
    let Some(raw) = args.get("file_path").and_then(Value::as_str) else {
        return Err("file_path must be a non-empty string".into());
    };
    if raw.trim().is_empty() {
        return Err("file_path must be a non-empty string".into());
    }
    // Only a non-blank path is checked: an empty `content` is legitimate, it
    // writes an empty file.
    let contents = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or("content must be a string")?
        .to_string();
    Ok(WriteRequest {
        path: display_path(root, raw),
        contents,
    })
}

/// Render a `write` result: the create/update confirmation envelope.
///
/// No content is echoed back, matching upstream, and `before`/`after` ride as
/// `meta` diffs so a UI's diff card survives replay.
pub fn write_outcome(
    display: &str,
    operation: &str,
    before: Option<&str>,
    after: &str,
    diffs: Value,
) -> Outcome {
    let verb = if operation == "create" {
        "Created"
    } else {
        "Updated"
    };
    let text =
        format!("<path>{display}</path>\n<type>file</type>\n<content>\n{verb} file\n</content>");
    let _ = (before, after);
    Outcome::text(text).with_meta(json!({ "diffs": diffs }))
}

/// An `edit` request after argument validation.
#[derive(Debug, Clone, PartialEq)]
pub struct EditRequest {
    pub path: String,
    pub old: String,
    pub new: String,
    pub replace_all: bool,
}

/// Parse `edit`'s arguments.
pub fn edit_request(arguments: &Arguments, root: &str) -> Result<EditRequest, String> {
    let Some(args) = arguments.as_object() else {
        return Err("invalid arguments: expected an object".into());
    };
    let Some(raw) = args.get("file_path").and_then(Value::as_str) else {
        return Err("file_path must be a non-empty string".into());
    };
    if raw.trim().is_empty() {
        return Err("file_path must be a non-empty string".into());
    }
    let old = args
        .get("old_string")
        .and_then(Value::as_str)
        .ok_or("old_string must be a non-empty string")?
        .to_string();
    if old.is_empty() {
        return Err("old_string must be a non-empty string".into());
    }
    let new = args
        .get("new_string")
        .and_then(Value::as_str)
        .ok_or("new_string must be a string")?
        .to_string();
    if old == new {
        return Err("old_string and new_string must differ".into());
    }
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(EditRequest {
        path: display_path(root, raw),
        old,
        new,
        replace_all,
    })
}

/// Apply an edit to the file's text, or report why it cannot be applied.
///
/// The three refusals are upstream's, and each is distinct because the model's
/// correction differs: no match means a wrong `old_string`, several matches
/// means the same, and neither is a filesystem error.
#[derive(Debug, Clone, PartialEq)]
pub struct Edited {
    pub before: String,
    pub after: String,
}

pub fn apply_edit(text: &str, request: &EditRequest) -> Result<Edited, String> {
    // Matching is on the file as it is; only the *message* is upstream's. The
    // three texts are `applyLiteralEdit`'s verbatim, including the
    // `matched N times` wording that differs from the argument-validation
    // message above.
    let count = text.matches(&request.old).count();
    if count == 0 {
        return Err(format!("old_string was not found in \"{}\"", request.path));
    }
    if count > 1 && !request.replace_all {
        return Err(format!(
            "old_string matched {count} times in \"{}\"; provide a more specific old_string or set replace_all to true",
            request.path
        ));
    }
    let after = if request.replace_all {
        text.replace(&request.old, &request.new)
    } else {
        text.replacen(&request.old, &request.new, 1)
    };
    Ok(Edited {
        before: text.to_string(),
        after,
    })
}

/// Render an `edit` result: the confirmation sentence.
pub fn edit_outcome(
    display: &str,
    replace_all: bool,
    before: &str,
    after: &str,
    diffs: Value,
) -> Outcome {
    let text = if replace_all {
        format!("The file {display} has been updated. All occurrences were successfully replaced.")
    } else {
        format!("The file {display} has been updated successfully.")
    };
    let _ = (before, after);
    Outcome::text(text).with_meta(json!({ "diffs": diffs }))
}

/// The diff hunks a mutation's `meta` carries, for a UI's diff card.
///
/// A three-line-context hunk per applied change, matching `computeHunkDiffs`'s
/// `DIFF_CONTEXT`. `oldText` is `null` for a pure insertion. An identical pair
/// yields no hunks, which is what upstream gives for a no-op.
pub fn hunk_diffs(path: &str, before: &str, after: &str) -> Value {
    const CONTEXT: usize = 3;
    let b = split_lines(before);
    let a = split_lines(after);
    // The common prefix and suffix are the context's outer bounds; what lies
    // between them is the change.
    let mut prefix = 0;
    while prefix < b.len() && prefix < a.len() && b[prefix] == a[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < b.len() - prefix
        && suffix < a.len() - prefix
        && strip_cr(b[b.len() - 1 - suffix]) == strip_cr(a[a.len() - 1 - suffix])
    {
        suffix += 1;
    }
    let start = prefix.saturating_sub(CONTEXT);
    // The window extends past the change by up to `CONTEXT` lines, and no
    // further than the text it came from.
    let old_end = (b.len() - suffix + CONTEXT).min(b.len());
    let new_end = (a.len() - suffix + CONTEXT).min(a.len());
    let old_lines: Vec<&str> = b[start..old_end].to_vec();
    let new_lines: Vec<&str> = a[start..new_end].to_vec();
    if old_lines == new_lines {
        return json!([]);
    }
    let old_text = if old_lines.is_empty() {
        Value::Null
    } else {
        json!(old_lines.join("\n"))
    };
    json!([{ "path": path, "oldText": old_text, "newText": new_lines.join("\n") }])
}

/// What the driver answered an effect with, reduced to what a tool reads.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// A text read succeeded.
    Text(String),
    /// A write succeeded.
    Done,
    /// A `Stat` resolved the target: its canonical path, whether it is a
    /// directory, and the driver's version token.
    ///
    /// Carried rather than reduced to a bool because the *canonical* path is the
    /// whole point of the effect for the fence: containment is judged against it,
    /// not against the joined string (see [`super::sandbox`]). `version` is the
    /// observation basis the write guard compares against; `None` for a
    /// directory, whose bytes have no version.
    Stat {
        canonical: String,
        is_dir: bool,
        version: Option<String>,
    },
    /// The target does not exist. Distinct from [`Self::Failed`] because the
    /// tools branch on it: a `write` creates, a `read` refuses.
    NotFound,
    /// A `ProcessExec` settled. Every field is a fact the bash renderer needs
    /// and none is derivable from another: the exit code and the signal are
    /// distinct (a signal death has no code), and `truncated` is not inferable
    /// from the text.
    Process {
        exit_code: Option<i32>,
        signal: Option<i32>,
        stdout: String,
        stderr: String,
        truncated: bool,
        timed_out: bool,
        /// The turn was cancelled and the child was killed for it, distinct
        /// from its own deadline firing.
        aborted: bool,
        /// Where the untruncated stream spilled, when it overflowed.
        spill_path: Option<String>,
    },
    /// The effect failed, with the message the driver rendered.
    Failed(String),
    /// A `ProcessStart` answered: the child is running and its pid is the
    /// handle a later `ProcessRead`/`ProcessKill` names.
    ProcessStarted { pid: u32 },
    /// A `ProcessRead` answered: the unread output deltas, and the settle
    /// state when the child has finished.
    ProcessChunk {
        running: bool,
        stdout_delta: String,
        stderr_delta: String,
        /// Settle facts, all absent while `running`.
        exit_code: Option<i32>,
        signal: Option<i32>,
        aborted: bool,
    },
}

/// The renderer for an effect that failed at the driver.
pub fn failed_answer(message: String) -> Outcome {
    Outcome::denied(message)
}

/// What the fence needs to know about a call before it may run.
///
/// **A plain mutation does not ask.** This is the correction the corpus forced,
/// and it is worth stating because the obvious design — "a mutating tool needs
/// approval" — is wrong in a way that is easy to ship: `fs-write`, `fs-edit` and
/// `session-sandbox-root` all call `write` or `edit` under an `ask` policy and
/// record **zero** `approval/asked` rows. Upstream reaches the approval seam from
/// `tools/pre-execute`, whose only registrants in the base profile are the two
/// escalation paths, and an escalation path only fires when the call carries
/// `sandbox_permissions`. So the ask is raised by a *request to widen*, not by
/// mutating.
///
/// A gate that asked on every write would fill the log with questions no tool
/// asked and make the audit trail meaningless — which is precisely the trap this
/// module's earlier draft fell into.
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    /// Run the call; no approval question is owed.
    Allow,
    /// Put one question to the approval chain first. `reason` is upstream's
    /// audit text, so the `approval/asked` row reads the same as the control's.
    Ask { reason: String },
    /// Refuse without asking — a malformed escalation, which never reaches a
    /// human because it is not a decision anyone can make.
    Refuse(Outcome),
}

/// Decide a call's gate against the session's fence.
///
/// The escalation ladder is checked *here*, before any question is put, because
/// a request that is not strictly wider is not a decision a human can make: it
/// asks for what the call already has. Upstream checks the same thing at
/// execution and never prompts for a non-widening request.
pub fn gate(name: &str, arguments: &Arguments, fence: &Fence) -> Gate {
    if name == "read" {
        // Reading confined content is not what a prompt exists to guard.
        return Gate::Allow;
    }
    let Ok(args) = arguments.as_object().ok_or(()) else {
        // Arguments that are not an object cannot carry an escalation, and the
        // tool's own parser reports the malformation.
        return Gate::Allow;
    };
    let permissions = args.get("sandbox_permissions").and_then(Value::as_str);
    let justification = args.get("justification").and_then(Value::as_str);

    // The pairing rules a schema cannot express, so they are checked here.
    if permissions.is_some() && justification.is_none() {
        return Gate::Refuse(Outcome::denied(
            "invalid escalation: sandbox_permissions requires a justification",
        ));
    }
    if justification.is_some() && permissions.is_none() {
        return Gate::Refuse(Outcome::denied(
            "invalid escalation: justification is only valid together with sandbox_permissions",
        ));
    }
    if let Some(j) = justification
        && j.trim().is_empty()
    {
        return Gate::Refuse(Outcome::denied(
            "invalid justification: expected a non-empty sentence",
        ));
    }
    let Some(requested) = permissions else {
        return Gate::Allow;
    };
    // Present by the pairing rule above; a request without a reason was already
    // refused, so this cannot be `None`.
    let reason = justification.unwrap_or_default();

    let Some(target) = Mode::parse(requested) else {
        return Gate::Refuse(Outcome::denied(format!(
            "sandbox escalation to \"{requested}\" is not a known mode (expected one of {})",
            Mode::targets()
                .iter()
                .map(|m| format!("\"{}\"", m.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    match widening(fence.mode(), target) {
        // The one case that reaches a human. The reason text is
        // `approveEscalation`'s, so the audit row matches the control's.
        Escape::Wider => Gate::Ask {
            reason: format!("escalate sandbox to {}: {reason}", target.as_str()),
        },
        Escape::Same => Gate::Refuse(Outcome::denied(format!(
            "sandbox escalation to \"{requested}\" is not strictly wider than this call's current \"{}\" mode",
            fence.mode().as_str()
        ))),
        Escape::Narrower => Gate::Refuse(Outcome::denied(format!(
            "sandbox escalation to \"{requested}\" is narrower than this call's current \"{}\" mode",
            fence.mode().as_str()
        ))),
    }
}

/// The mode a granted escalation stamps onto one call.
///
/// The grant is *one-shot*: it widens the fence for this call only, which is why
/// it is returned rather than mutating the session's fence.
pub fn escalated_fence(fence: &Fence, requested: &str) -> Fence {
    match Mode::parse(requested) {
        Some(mode) => Fence::new(mode, fence.root.clone()),
        None => fence.clone(),
    }
}

/// The confinement refusal, as `super::sandbox` renders it.
///
/// Carries the structured `FS_SANDBOX_DENIED` code, which is load-bearing rather
/// than decorative: upstream's comment on the mapping says why —
///
/// > `ToolRuntime` populates `result.error` only for `HarnessError` instances, so
/// > a plain `Error` would strip the code retry/observers key off.
///
/// That is, the code is what a *reader* of the log keys a retry or an observer
/// off. A denial whose text is right but whose code is absent is a denial no
/// program can act on, and the text alone would leave the two enforcement
/// families (filesystem and shell) distinguishable only by a string match.
pub fn confinement(denial: &Denial) -> Outcome {
    Outcome::denied(denial.message()).with_error(fs_error("FS_SANDBOX_DENIED"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Arguments {
        Arguments::parse(&v.to_string())
    }

    /// The envelope, numbering and footer are `formatReadOutput`'s, verbatim.
    ///
    /// This is the exact text the committed `fs-read` snapshot's `tool/result`
    /// carries, which is why it is quoted rather than described.
    #[test]
    fn a_read_renders_the_upstream_envelope() {
        let w = ReadWindow {
            path: "/w/greeting.txt".into(),
            offset: 1,
            limit: READ_LIMIT,
        };
        let out = read_outcome(&w, "hello\n");
        let text = out.content[0]["text"].as_str().unwrap();
        assert_eq!(
            text,
            "<path>/w/greeting.txt</path>\n<type>file</type>\n<content>\n1: hello\n\n(End of file - total 1 lines)\n</content>"
        );
        assert_eq!(out.meta.as_ref().unwrap()["totalLines"], 1);
        // A `.png` is not a text language, but a `.txt` is not in the map either.
        assert!(out.meta.as_ref().unwrap().get("lang").is_none());
    }

    /// A `lang` hint is derived from the *display* path, and a dotfile has none.
    #[test]
    fn a_language_hint_comes_from_the_extension() {
        assert_eq!(lang_from_path("/w/a.rs"), Some("rs"));
        assert_eq!(lang_from_path("/w/a.YAML"), Some("yaml"));
        assert_eq!(lang_from_path("/w/.gitignore"), None);
        assert_eq!(lang_from_path("/w/Makefile"), None);
        // A prototype key is not a language: the map is a match, so this is
        // structural rather than a lookup that could see `Object.prototype`.
        assert_eq!(lang_from_path("/w/a.constructor"), None);
    }

    /// A window's footer changes with the window, not only with the file.
    #[test]
    fn a_windowed_read_reports_how_to_continue() {
        let text = (1..=10).map(|i| format!("line {i}\n")).collect::<String>();
        let w = ReadWindow {
            path: "/w/f.txt".into(),
            offset: 3,
            limit: 2,
        };
        let out = read_outcome(&w, &text);
        let body = out.content[0]["text"].as_str().unwrap();
        assert!(body.contains("3: line 3"), "{body}");
        assert!(body.contains("4: line 4"), "{body}");
        assert!(!body.contains("5: line 5"), "{body}");
        assert!(
            body.contains("(Showing lines 3-4 of 10. Use offset=5 to continue.)"),
            "{body}"
        );
    }

    /// An `offset` past EOF is a refusal, not an empty window.
    ///
    /// This is `finish`'s check, and losing it would make a typo'd offset look
    /// like an empty file — a silent wrong answer instead of a stated one.
    #[test]
    fn an_offset_past_the_end_is_refused() {
        let w = ReadWindow {
            path: "/w/f.txt".into(),
            offset: 9,
            limit: READ_LIMIT,
        };
        let out = read_outcome(&w, "a\nb\n");
        assert!(out.is_error, "{out:?}");
        let text = out.content[0]["text"].as_str().unwrap();
        assert!(
            text.contains("offset 9 is out of range for \"/w/f.txt\" (2 lines)"),
            "{text}"
        );
        // An empty file at offset 1 is *not* out of range.
        let ok = read_outcome(
            &ReadWindow {
                path: "/w/empty".into(),
                offset: 1,
                limit: READ_LIMIT,
            },
            "",
        );
        assert!(!ok.is_error, "{ok:?}");
    }

    /// A `\r\n` file reads without its carriage returns, and an unterminated
    /// final line still counts.
    #[test]
    fn lines_are_split_the_way_the_window_builder_splits_them() {
        let w = ReadWindow {
            path: "/w/f".into(),
            offset: 1,
            limit: READ_LIMIT,
        };
        let out = read_outcome(&w, "a\r\nb");
        let text = out.content[0]["text"].as_str().unwrap();
        assert!(text.contains("1: a\n2: b"), "{text}");
        assert!(text.contains("total 2 lines"), "{text}");
    }

    /// A `read` with no `file_path` is refused by name.
    #[test]
    fn a_missing_or_blank_path_is_refused() {
        assert!(read_window(&args(json!({})), "/w").is_err());
        assert!(read_window(&args(json!({ "file_path": "  " })), "/w").is_err());
        let e = read_window(&args(json!({ "file_path": "a", "limit": 99999 })), "/w").unwrap_err();
        assert!(
            e.contains("limit must be less than or equal to 2000"),
            "{e}"
        );
        assert!(read_window(&args(json!({ "file_path": "a", "offset": 0 })), "/w").is_err());
    }

    /// A relative path joins the workspace root; an absolute one does not.
    #[test]
    fn a_path_resolves_against_the_workspace() {
        assert_eq!(display_path("/w", "a.txt"), "/w/a.txt");
        assert_eq!(display_path("/w", "/etc/hosts"), "/etc/hosts");
        assert_eq!(display_path("/w", "nested/a.txt"), "/w/nested/a.txt");
    }

    /// A write's confirmation is `formatWriteOutput`'s, and its `before` becomes
    /// a diff hunk rather than being echoed.
    #[test]
    fn a_write_reports_created_or_updated() {
        let created = write_outcome(
            "/w/a.txt",
            "create",
            None,
            "hi",
            hunk_diffs("/w/a.txt", "", "hi"),
        );
        assert_eq!(
            created.content[0]["text"].as_str().unwrap(),
            "<path>/w/a.txt</path>\n<type>file</type>\n<content>\nCreated file\n</content>"
        );
        // A pure insertion has no old side.
        assert_eq!(
            created.meta.as_ref().unwrap()["diffs"][0]["oldText"],
            Value::Null
        );
        assert_eq!(created.meta.as_ref().unwrap()["diffs"][0]["newText"], "hi");

        let updated = write_outcome(
            "/w/a.txt",
            "update",
            Some("old"),
            "new",
            hunk_diffs("/w/a.txt", "old", "new"),
        );
        assert!(
            updated.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("Updated file"),
            "{updated:?}"
        );
        assert_eq!(updated.meta.as_ref().unwrap()["diffs"][0]["oldText"], "old");
    }

    /// The two edit sentences are `formatEditOutput`'s, and they differ.
    #[test]
    fn an_edit_says_which_kind_of_replacement_ran() {
        let one = edit_outcome("/w/a.txt", false, "a", "b", json!([]));
        assert_eq!(
            one.content[0]["text"].as_str().unwrap(),
            "The file /w/a.txt has been updated successfully."
        );
        let all = edit_outcome("/w/a.txt", true, "a", "b", json!([]));
        assert_eq!(
            all.content[0]["text"].as_str().unwrap(),
            "The file /w/a.txt has been updated. All occurrences were successfully replaced."
        );
    }

    /// The three refusals are distinct, and a repeated match needs `replace_all`.
    #[test]
    fn an_edit_refuses_by_name() {
        let req = EditRequest {
            path: "/w/a.txt".into(),
            old: "x".into(),
            new: "y".into(),
            replace_all: false,
        };
        let e = apply_edit("a\nb\n", &req).unwrap_err();
        assert!(e.contains("old_string was not found"), "{e}");

        let e = apply_edit("x x\n", &req).unwrap_err();
        assert!(e.contains("matched 2 times"), "{e}");
        assert!(e.contains("replace_all to true"), "{e}");

        // With `replace_all`, both are replaced.
        let all = EditRequest {
            path: "/w/a.txt".into(),
            old: "x".into(),
            new: "y".into(),
            replace_all: true,
        };
        assert_eq!(apply_edit("x x\n", &all).unwrap().after, "y y\n");

        // Without `replace_all`, a unique match is replaced and a repeated one is
        // refused — there is no "replace just the first" mode, which is upstream's
        // contract (`countOccurrences`, then a whole-content split/join).
        let unique = EditRequest {
            path: "/w/a.txt".into(),
            old: "x".into(),
            new: "z".into(),
            replace_all: false,
        };
        assert_eq!(apply_edit("x y\n", &unique).unwrap().after, "z y\n");
    }

    /// An edit's own argument rules: a blank path, an empty `old_string`, and an
    /// equal pair are each refused.
    #[test]
    fn an_edit_validates_its_arguments() {
        let base = json!({ "file_path": "a.txt", "old_string": "x", "new_string": "y" });
        assert!(edit_request(&args(base.clone()), "/w").is_ok());
        assert!(
            edit_request(
                &args(json!({ "file_path": "", "old_string": "x", "new_string": "y" })),
                "/w"
            )
            .is_err()
        );
        assert!(
            edit_request(
                &args(json!({ "file_path": "a", "old_string": "", "new_string": "y" })),
                "/w"
            )
            .is_err()
        );
        let same = json!({ "file_path": "a", "old_string": "x", "new_string": "x" });
        assert!(
            edit_request(&args(same), "/w")
                .unwrap_err()
                .contains("must differ")
        );
    }

    /// No `file_path` and no `content` on a write are refused by name.
    #[test]
    fn a_write_validates_its_arguments() {
        assert!(write_request(&args(json!({ "file_path": "a" })), "/w").is_err());
        assert!(write_request(&args(json!({ "content": "x" })), "/w").is_err());
        // An empty content is legitimate.
        let ok = write_request(&args(json!({ "file_path": "a", "content": "" })), "/w").unwrap();
        assert_eq!(ok.contents, "");
        assert_eq!(ok.path, "/w/a");
    }

    /// Unparseable arguments are preserved as text, not silently emptied.
    #[test]
    fn unparseable_arguments_are_preserved() {
        let a = Arguments::parse("{not json");
        assert_eq!(a, Arguments::NotAnObject("{not json".into()));
        assert!(a.as_object().is_none());
        // Empty input maps to `{}`, matching upstream.
        assert_eq!(Arguments::parse("").as_object().unwrap().len(), 0);
        // A non-object renders back as the text the model typed.
        assert_eq!(Arguments::parse("42"), Arguments::NotAnObject("42".into()));
    }

    /// The three approval outcomes that are not grants get distinct text.
    #[test]
    fn a_denied_approval_says_which_kind_of_no_it_was() {
        let r = approval_denial("write", "rejected");
        assert_eq!(
            r.content[0]["text"].as_str().unwrap(),
            "Error: the user rejected tool \"write\""
        );
        let c = approval_denial("write", "cancelled");
        assert!(
            c.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("was cancelled")
        );
        let u = approval_denial("write", "unavailable");
        assert!(
            u.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("no approval channel is available")
        );
        // **A denial is an error result, and its structured info lives on
        // `data.error` — not on the part, and not as a message.** The corpus is
        // unambiguous about both halves: of 107 recorded `tool/result` rows, five
        // carry `error` and every one has it on `data` shaped `{name, code}`.
        assert!(r.is_error);
        assert!(
            r.error.is_none(),
            "an approval denial is the gate's own judgment, so it has no upstream error class"
        );
        assert_eq!(
            r.content[0]["text"],
            json!("Error: the user rejected tool \"write\"")
        );
    }

    /// **A plain mutation does not ask.** The corpus is the authority here:
    /// `fs-write`, `fs-edit` and `session-sandbox-root` each call a mutator under
    /// an `ask` policy and record zero asks, so a gate that asked on every write
    /// would log questions no tool asked.
    #[test]
    fn a_plain_mutation_runs_without_asking() {
        let f = Fence::new(Mode::WorkspaceWrite, "/w");
        assert_eq!(
            gate(
                "write",
                &args(json!({ "file_path": "a", "content": "x" })),
                &f
            ),
            Gate::Allow
        );
        assert_eq!(
            gate(
                "edit",
                &args(json!({ "file_path": "a", "old_string": "x", "new_string": "y" })),
                &f
            ),
            Gate::Allow
        );
        assert_eq!(
            gate("read", &args(json!({ "file_path": "a" })), &f),
            Gate::Allow
        );
    }

    /// The one ask this host can raise is a request to widen, and its reason is
    /// `approveEscalation`'s audit text verbatim.
    #[test]
    fn a_widening_request_asks_with_the_upstream_reason() {
        let f = Fence::new(Mode::ReadOnly, "/w");
        let g = gate(
            "write",
            &args(json!({
                "file_path": "a",
                "content": "x",
                "sandbox_permissions": "workspace-write",
                "justification": "the user asked me to write outside the read-only root",
            })),
            &f,
        );
        assert_eq!(
            g,
            Gate::Ask {
                reason: "escalate sandbox to workspace-write: the user asked me to write outside the read-only root".into()
            }
        );
        // A granted escalation stamps the wider mode onto that one call.
        let widened = escalated_fence(&f, "workspace-write");
        assert_eq!(widened.mode(), Mode::WorkspaceWrite);
        assert!(widened.allows(Path::new("/w/a.txt")));
        // The session's own fence is untouched by the one-shot grant.
        assert_eq!(f.mode(), Mode::ReadOnly);
    }

    /// A malformed escalation is refused **without** asking, because it is not a
    /// decision anyone can make.
    #[test]
    fn a_malformed_escalation_is_refused_not_asked() {
        let f = Fence::new(Mode::WorkspaceWrite, "/w");
        let unpaired = gate(
            "write",
            &args(json!({ "file_path": "a", "sandbox_permissions": "danger-full-access" })),
            &f,
        );
        assert!(matches!(unpaired, Gate::Refuse(_)), "{unpaired:?}");
        let Gate::Refuse(o) = unpaired else {
            unreachable!()
        };
        assert!(
            o.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("requires a justification"),
            "{o:?}"
        );

        let lone = gate(
            "write",
            &args(json!({ "file_path": "a", "justification": "because" })),
            &f,
        );
        assert!(matches!(lone, Gate::Refuse(_)));

        let blank = gate(
            "write",
            &args(json!({
                "file_path": "a",
                "sandbox_permissions": "danger-full-access",
                "justification": "   ",
            })),
            &f,
        );
        assert!(matches!(blank, Gate::Refuse(_)));

        // A request for the mode already in effect is not wider, so it is not a
        // question — granting it would mint "approval" for an operation that
        // needed none.
        let same = gate(
            "write",
            &args(json!({
                "file_path": "a",
                "sandbox_permissions": "workspace-write",
                "justification": "I need more room",
            })),
            &f,
        );
        let Gate::Refuse(o) = same else {
            panic!("a non-widening request must not reach a human: {same:?}")
        };
        assert!(
            o.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("not strictly wider"),
            "{o:?}"
        );

        let bogus = gate(
            "write",
            &args(json!({
                "file_path": "a",
                "sandbox_permissions": "root",
                "justification": "I need more room",
            })),
            &f,
        );
        let Gate::Refuse(o) = bogus else {
            unreachable!()
        };
        assert!(
            o.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("not a known mode"),
            "{o:?}"
        );
    }

    /// The diff metadata is the shape a UI card reads on replay.
    #[test]
    fn a_diff_hunk_carries_context_around_the_change() {
        let before = "a\nb\nc\nd\ne\nf\ng\n";
        let after = "a\nb\nc\nD\ne\nf\ng\n";
        let d = hunk_diffs("f.txt", before, after);
        assert_eq!(d[0]["path"], "f.txt");
        let old = d[0]["oldText"].as_str().unwrap();
        let new = d[0]["newText"].as_str().unwrap();
        assert!(old.contains("d"), "{old}");
        assert!(new.contains("D"), "{new}");
        // Three lines of context on either side.
        assert!(old.starts_with("a\nb\nc"), "{old}");
        assert!(old.ends_with("e\nf\ng"), "{old}");
        // An unchanged pair has no hunk at all.
        assert_eq!(hunk_diffs("f.txt", before, before), json!([]));
    }

    /// A tool result row is user-role, cites its call, and carries the surface
    /// bookkeeping the corpus shows on every one.
    #[test]
    fn a_result_row_cites_its_call() {
        let call = Call {
            id: "call_1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        };
        let data = Outcome::text("hi").row_data(2, 3, &call, 14);
        assert_eq!(data["turn"], 2);
        assert_eq!(data["step"], 3);
        assert_eq!(data["message"]["role"], "user");
        assert_eq!(data["message"]["source"]["callId"], "call_1");
        assert_eq!(data["sourceEventSeqs"][0], 14);
        assert_eq!(data["surfaceOp"], "append");
        let part = &data["message"]["content"][0];
        assert_eq!(part["type"], "tool-result");
        assert_eq!(part["toolCallId"], "call_1");
        assert_eq!(part["isError"], false);

        let call_row = call.row_data(2, 3);
        assert_eq!(call_row["callId"], "call_1");
        assert_eq!(call_row["name"], "read");
    }

    /// **`error` is a sibling of the message, not a field of it, and it is
    /// `{name, code}` rather than a message.**
    ///
    /// Both halves are fixed by the corpus. Across 107 recorded `tool/result`
    /// rows, five carry an `error` key; every one is on `data` (never on the
    /// message part) and every one has exactly the two keys `name` and `code` —
    /// `FsError`/`FS_NOT_FOUND`, `FsError`/`FS_NOT_OBSERVED`,
    /// `SandboxUnavailableError`/`SANDBOX_UNAVAILABLE`,
    /// `WebError`/`WEB_PROVIDER_ERROR`. A `{message}` object on the part is
    /// therefore wrong twice over, and it is what this module shipped first.
    #[test]
    fn a_structured_error_is_a_sibling_of_the_message_and_names_a_class() {
        let call = Call {
            id: "call_1".into(),
            name: "write".into(),
            arguments: "{}".into(),
        };
        let data = Outcome::denied("nope")
            .with_error(sandbox_unavailable())
            .row_data(1, 1, &call, 9);

        // On `data`, beside `message`.
        assert_eq!(
            data["error"],
            json!({ "name": "SandboxUnavailableError", "code": "SANDBOX_UNAVAILABLE" })
        );
        assert_eq!(
            data["error"].as_object().unwrap().len(),
            2,
            "two keys, no third"
        );
        // And *not* on the part.
        assert!(data["message"]["content"][0].get("error").is_none());
        // The text the model reads is still the content block, which is why
        // `error` need not carry a message.
        assert_eq!(
            data["message"]["content"][0]["content"][0]["text"],
            json!("Error: nope")
        );

        // A `data.error`'s presence is independent of `isError` being true for
        // the *part*: the corpus's `fs-policy-reject` is both, but the honest
        // statement is that the classification is what a program reads.
        assert_eq!(data["message"]["content"][0]["isError"], json!(true));
    }

    /// The catalog is a closed set, and every entry has a name and a schema.
    #[test]
    fn the_catalog_is_small_and_executable() {
        let names: Vec<&str> = catalog().iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec!["read", "write", "edit", "bash", "job_output", "job_list", "job_kill"]
        );
        for spec in catalog() {
            assert_eq!(spec.input_schema["type"], "object");
        }
        // `job_list` has no properties; every other spec advertises at least
        // one — the catalog test's non-empty assertion is what makes a tool's
        // schema real rather than a shell.
        for spec in catalog().iter().filter(|t| t.name != "job_list") {
            assert!(
                spec.input_schema["properties"]
                    .as_object()
                    .is_some_and(|p| !p.is_empty()),
                "{} advertises no properties",
                spec.name
            );
        }
        // The fs family resolves a path; `bash` takes a command instead, so the
        // `file_path` property is a fact of those three and not of the catalog.
        for spec in &catalog()[..3] {
            assert!(
                spec.input_schema["properties"]["file_path"].is_object(),
                "{} has no file_path",
                spec.name
            );
        }
        // **The escalation fields ARE advertised, on the two mutators and only
        // there.** `tool-fs` spreads `schemaFields()` into exactly `write` and
        // `edit` when a confining backend is mounted, and this host's filesystem
        // backend confines. An earlier version of this test asserted the
        // opposite — that nothing here advertises a wider mode — which was the
        // defect rather than the contract: the gate already understood
        // escalation, so withholding the fields left the ladder unreachable.
        //
        // `read` is excluded because it mutates nothing, so there is no denial
        // it could escalate out of.
        let write = &catalog()[1].input_schema;
        let edit = &catalog()[2].input_schema;
        for (name, schema) in [("write", write), ("edit", edit)] {
            assert_eq!(
                schema["properties"]["sandbox_permissions"]["enum"],
                json!(["workspace-write", "danger-full-access"]),
                "{name} must offer the closed escalation vocabulary"
            );
            assert!(
                schema["properties"]["justification"].is_object(),
                "{name} must offer the justification the pair requires"
            );
        }
        assert!(
            catalog()[0].input_schema["properties"]
                .get("sandbox_permissions")
                .is_none(),
            "read mutates nothing, so it has no denial to escalate"
        );
        assert!(tool_definitions()[0].name == "read");
    }

    /// The escalation vocabulary a model is *offered* is the one the gate
    /// *validates*, and neither includes the floor.
    ///
    /// `read-only` is absent from both because it is the narrowest mode: a
    /// request to escalate to it asks for nothing, and [`widening`] would call it
    /// `Narrower`. A drift between the two lists would be a schema offering a
    /// target the gate then refuses as malformed — a call the model could not
    /// have known was invalid.
    #[test]
    fn the_advertised_targets_are_the_validated_ones() {
        for name in ["write", "edit"] {
            let specs = catalog();
            let spec = specs.iter().find(|t| t.name == name).unwrap();
            let advertised: Vec<&str> =
                spec.input_schema["properties"]["sandbox_permissions"]["enum"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
            let validated: Vec<&str> = Mode::targets().iter().map(|m| m.as_str()).collect();
            assert_eq!(
                advertised, validated,
                "{name}'s enum must match Mode::targets"
            );
            assert!(
                !advertised.contains(&"read-only"),
                "the floor is not a target"
            );

            // And every advertised target really is strictly wider than the
            // narrowest mode a session can start in — the property the gate
            // checks per call.
            for target in &advertised {
                assert_eq!(
                    widening(Mode::ReadOnly, Mode::parse(target).unwrap()),
                    Escape::Wider
                );
            }
        }
    }

    /// `is_mutating` is the gate's whole policy, so it is asserted rather than
    /// inferred from the call sites.
    #[test]
    fn only_the_two_mutators_need_approval() {
        assert!(is_mutating("write"));
        assert!(is_mutating("edit"));
        assert!(!is_mutating("read"));
        assert!(!is_mutating("bash"));
        // The job controls touch no workspace bytes: a kill stops a process,
        // which is upstream's own treatment (tool-jobs registers no
        // `tools/pre-execute` escalation path).
        for name in ["job_output", "job_list", "job_kill"] {
            assert!(!is_mutating(name), "{name} must not ask");
        }
    }
}
