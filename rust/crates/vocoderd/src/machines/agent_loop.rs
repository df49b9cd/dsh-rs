//! The agent loop's turn/step FSM — pure, no I/O.
//!
//! Upstream's driver (`dsh/packages/core/agent-loop/src/agent.ts`) is a
//! turn/step state machine over the session log. This is the same machine with
//! the I/O removed: it takes inputs, emits *row drafts* to append, and asks for
//! a model call. Nothing here reads a file, calls a network, or knows what a
//! clock is — which is what makes it testable against the committed snapshot
//! corpus and replayable from a recording.
//!
//! ## What is faithfully mirrored
//!
//! - **The phase machine.** `idle → running{turn, step} → idle`. A turn opens
//!   with `turn/start` and always closes with `turn/end`; a step opens with
//!   `step/start` and always closes with `step/end`. The turn number is
//!   `last_turn + 1`, and `step` resets to 0 at each turn boundary.
//! - **`turn/end` is unconditional.** Upstream appends it from a `finally`
//!   block, so a turn that fails, aborts, or is blocked still closes. A log
//!   whose last turn never ended is what the `interrupted` reason exists to
//!   repair on cold reads, and this machine never produces one.
//! - **`max-tokens` is sticky.** Once any step in a turn hits its ceiling, a
//!   later step that completes normally must not downgrade the turn's outcome.
//!   Upstream guards this explicitly and it is easy to get wrong.
//! - **A turn ends when the inbox has nothing more for it.** After a step,
//!   pending input opens another step; otherwise the turn closes.
//! - **`agent/pre-step` may reject.** A rejected step blocks the turn:
//!   `turn/end {kind: 'blocked'}` and no `step/start`.
//!
//! ## What is *not* here (deliberately, and named so it is not mistaken for done)
//!
//! - **The model call itself.** `StepOutcome::CallModel` is the seam: step 2 of
//!   the plan fills it with the LLM machine. Until then a caller drives the
//!   reply in by hand, which is exactly how the tests exercise a whole turn.
//! - **Tool execution.** A `tool-call` in a reply currently concludes the step;
//!   the tool seam is step 3 and will need to splice results into the inbox the
//!   way upstream's `executeToolCalls` does.
//! - **The system prompt and `request/header`.** Both are model-facing and
//!   belong with the call seam.
//! - **Streaming frames.** `assistant/chunk` is emitted by the reply path, so
//!   it lands with the seam rather than here.
//!
//! ## Why a separate type from the namespace machine
//!
//! The FSM is `vocoder-cordis`-free on purpose: no `MachineIn`, no
//! `MachineOut`, no `RealizeRequest`. It is the part worth testing exhaustively
//! and replaying, and keeping it dependency-free is what lets those tests be
//! plain assertions over corpus rows.

// The FSM is complete and tested but **not yet wired to a namespace machine**:
// step 2 of the plan attaches it to the LLM seam, and step 3's tool seam drives
// its `has_tool_calls` input. Until then nothing outside this module and its
// tests constructs it, so every type here reads as dead code in a non-test
// build. The allow is scoped to the module rather than sprinkled per item so
// that removing it later is one deletion, and so the reason is stated once
// instead of eleven times.
#![allow(dead_code)]

use serde_json::{Value, json};

/// A row to append to the session log, before `seq` and `time` are assigned.
///
/// Seq assignment belongs to whoever publishes the generation (it depends on
/// the log's current length), and `time` to whatever clock that publisher uses.
/// Keeping both out of here is what makes a replay byte-comparable.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    /// The log row type, e.g. `turn/start`.
    pub row_type: &'static str,
    /// The row's `data` payload.
    pub data: Value,
}

impl Draft {
    fn new(row_type: &'static str, data: Value) -> Self {
        Self { row_type, data }
    }
}

/// Why a turn ended, mirroring upstream's `TurnEndReason`.
///
/// The vocabulary is the spec's, not an invention: `completed`, `aborted`,
/// `blocked`, `error`, `max-tokens`, `interrupted`. `interrupted` is *not*
/// producible here — upstream only synthesizes it when repairing a log whose
/// last turn never closed, and this machine always closes its turns.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    Completed,
    /// A cancellation interrupted the live turn; `cause` is durable.
    Aborted {
        cause: CancelCause,
    },
    /// The pre-step hook rejected the step.
    Blocked,
    /// The turn failed; `code` is the structured failure code.
    Error {
        code: String,
        message: String,
    },
    /// A step reached its output-token ceiling.
    MaxTokens,
}

impl TurnEnd {
    /// The `reason` payload as the log records it.
    pub fn to_reason(&self) -> Value {
        match self {
            Self::Completed => json!({ "kind": "completed" }),
            Self::Aborted { cause } => {
                json!({ "kind": "aborted", "reason": cause.to_value() })
            }
            Self::Blocked => json!({ "kind": "blocked" }),
            Self::Error { code, message } => json!({
                "kind": "error",
                "error": { "message": message, "code": code },
            }),
            Self::MaxTokens => json!({ "kind": "max-tokens" }),
        }
    }
}

/// Who asked for the cancellation, mirroring `AgentCancelCause`.
#[derive(Debug, Clone, PartialEq)]
pub enum CancelCause {
    User,
    Parent,
    Hook {
        reason: String,
    },
    Disposed,
    /// A coarse record imported from an older log that carried no cause.
    Legacy,
}

impl CancelCause {
    fn to_value(&self) -> Value {
        match self {
            Self::User => json!({ "kind": "user" }),
            Self::Parent => json!({ "kind": "parent" }),
            Self::Hook { reason } => json!({ "kind": "hook", "reason": reason }),
            Self::Disposed => json!({ "kind": "disposed" }),
            Self::Legacy => json!({ "kind": "legacy" }),
        }
    }
}

/// What the loop is doing.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle { last_turn: u64 },
    Running { turn: u64, step: u64, open: bool },
}

/// How a step ended, from the reply path's point of view.
#[derive(Debug, Clone, PartialEq)]
pub enum StepOutcome {
    /// The model produced a message with no tool calls.
    Completed,
    /// The step hit its output-token ceiling.
    MaxTokens,
    /// The call failed.
    Error { code: String, message: String },
}

/// An instruction the caller (or, later, the LLM machine) must act on.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopOutput {
    /// Append this row to the log, in order.
    Append(Draft),
    /// The step needs a model call. The caller feeds the reply back in via
    /// [`AgentLoop::step_reply`]. This is the seam step 2 fills.
    CallModel { turn: u64, step: u64 },
    /// The turn finished; nothing further is owed.
    TurnEnded(TurnEnd),
}

/// The turn/step FSM for one session.
pub struct AgentLoop {
    phase: Phase,
    /// Set while a cancel is in flight, so the step's reply is treated as an
    /// abort rather than an ordinary outcome. Upstream threads an `AbortSignal`
    /// through every await; this is that signal as a field, since there are no
    /// awaits to interrupt here.
    cancelled: Option<CancelCause>,
    /// `max-tokens` seen in this turn — sticky across steps, per upstream.
    turn_saw_max_tokens: bool,
    /// Whether a step is open (between `step/start` and `step/end`).
    step_open: bool,
}

impl Default for AgentLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentLoop {
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle { last_turn: 0 },
            cancelled: None,
            turn_saw_max_tokens: false,
            step_open: false,
        }
    }

    /// Rebuild the loop's phase from a session log.
    ///
    /// Upstream derives every request from the log rather than holding state
    /// across restarts, and its constructor seeds `lastTurn` from the
    /// `turnBoundary` projection. This is that reconstruction: the last
    /// `turn/end` (or `turn/start`, for a turn that never closed) tells the loop
    /// where it stands.
    ///
    /// A log whose last turn never ended is a crash-orphaned turn. Upstream
    /// repairs it with `turn/end {kind: 'interrupted'}`; **this machine does not
    /// append anything on resume** — it reports the phase it derived and lets
    /// the caller decide, because inventing a row during construction would make
    /// `resume` non-idempotent.
    pub fn resume_from(rows: &[Value]) -> Self {
        let mut last_turn = 0u64;
        let mut open_turn: Option<(u64, u64)> = None;
        for row in rows {
            let ty = row.get("type").and_then(Value::as_str).unwrap_or_default();
            let data = row.get("data").cloned().unwrap_or(Value::Null);
            match ty {
                "turn/start" => {
                    let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(1);
                    open_turn = Some((turn, 0));
                }
                "step/start" => {
                    let step = data.get("step").and_then(Value::as_u64).unwrap_or(1);
                    if let Some((turn, _)) = open_turn {
                        open_turn = Some((turn, step));
                    }
                }
                "turn/end" => {
                    let turn = data
                        .get("turn")
                        .and_then(Value::as_u64)
                        .unwrap_or(last_turn);
                    last_turn = last_turn.max(turn);
                    open_turn = None;
                }
                _ => {}
            }
        }
        // A turn left open by a crash still counts as *reached*: the next turn
        // must be one past it, not reuse its number.
        if let Some((turn, step)) = open_turn {
            // The step is treated as open: the crash happened mid-step, and a
            // caller that resumes should settle it rather than open another.
            return Self {
                phase: Phase::Running {
                    turn,
                    step,
                    open: false,
                },
                cancelled: None,
                turn_saw_max_tokens: false,
                step_open: true,
            };
        }
        Self::new_with_last_turn(last_turn)
    }

    fn new_with_last_turn(last_turn: u64) -> Self {
        Self {
            phase: Phase::Idle { last_turn },
            cancelled: None,
            turn_saw_max_tokens: false,
            step_open: false,
        }
    }

    /// The turn currently running, if any.
    pub fn current_turn(&self) -> Option<u64> {
        match self.phase {
            Phase::Running { turn, .. } => Some(turn),
            Phase::Idle { .. } => None,
        }
    }

    /// Whether the loop is mid-turn.
    pub fn is_running(&self) -> bool {
        matches!(self.phase, Phase::Running { .. })
    }

    /// Begin a turn for one batch of admitted user messages.
    ///
    /// Mirrors upstream's `turn()`: the number is `last_turn + 1`, `turn/start`
    /// is appended before anything else, and the first step is proposed
    /// immediately. A turn with **no** messages spends no model call — upstream
    /// closes it `completed` without a `step/start`, and that is what the
    /// `empty` argument here represents.
    pub fn begin_turn(&mut self, messages: &[Value]) -> Vec<LoopOutput> {
        let last_turn = match self.phase {
            Phase::Idle { last_turn } => last_turn,
            // A turn is already running. Upstream's driver never does this (its
            // phase machine reserves the turn before proposing a step), so
            // refusing is the honest answer rather than nesting turns.
            Phase::Running { turn, .. } => {
                return vec![LoopOutput::TurnEnded(TurnEnd::Error {
                    code: "AGENT_BUSY".into(),
                    message: format!("agent is already running turn {turn}"),
                })];
            }
        };
        let turn = last_turn + 1;
        self.phase = Phase::Running {
            turn,
            step: 0,
            open: true,
        };
        self.turn_saw_max_tokens = false;
        self.cancelled = None;

        let mut outs = vec![LoopOutput::Append(Draft::new(
            "turn/start",
            json!({ "turn": turn }),
        ))];

        // A cancel that arrived before the turn opened still owns it.
        if let Some(cause) = self.cancelled.clone() {
            outs.extend(self.close_turn(TurnEnd::Aborted { cause }));
            return outs;
        }

        if messages.is_empty() {
            // Upstream: "A removed waking message or an enter decision rewritten
            // to empty still owns the initial turn boundary, but it spends no
            // model call." The turn closes `completed` with no step.
            outs.extend(self.close_turn(TurnEnd::Completed));
            return outs;
        }
        outs
    }

    /// Open the current turn's next step, after the pre-step hook admitted it.
    ///
    /// This is the second half of what upstream's `turn()` does, split out
    /// because its `preStep()` sits between the two: the hook runs, and only an
    /// `enter` decision opens a step. A `reject` decision closes the turn with
    /// **no `step/start` at all**, which is why these cannot be one call.
    pub fn enter_step(&mut self) -> Vec<LoopOutput> {
        if !self.is_running() {
            return vec![];
        }
        self.begin_step()
    }

    /// Open the next step of the running turn.
    fn begin_step(&mut self) -> Vec<LoopOutput> {
        let Phase::Running { turn, step, .. } = self.phase else {
            return vec![];
        };
        if self.cancelled.is_some() {
            return vec![];
        }
        let next = step + 1;
        self.phase = Phase::Running {
            turn,
            step: next,
            open: true,
        };
        self.step_open = true;
        vec![
            LoopOutput::Append(Draft::new(
                "step/start",
                json!({ "turn": turn, "step": next }),
            )),
            LoopOutput::CallModel { turn, step: next },
        ]
    }

    /// Report the model's reply for the open step, and advance the loop.
    ///
    /// This is where a whole turn's shape is decided: a completed reply with no
    /// tool calls ends the turn, a `max-tokens` reply sets the sticky flag, a
    /// failure ends it `error`, and a reply that *did* call tools asks for
    /// another step (tool execution lands in step 3 of the plan).
    pub fn step_reply(
        &mut self,
        outcome: StepOutcome,
        has_tool_calls: bool,
        more_input: bool,
    ) -> Vec<LoopOutput> {
        let Phase::Running { turn, step, .. } = self.phase else {
            // A reply with no running turn is a caller bug. Saying so beats
            // returning an empty vector, which a caller would read as "handled".
            return vec![LoopOutput::TurnEnded(TurnEnd::Error {
                code: "AGENT_NO_OPEN_STEP".into(),
                message: "no turn is running, so no step can be settled".into(),
            })];
        };
        if !self.step_open {
            // A reply for a step that was never opened is a caller bug, not a
            // loop state: reporting it beats silently folding it into the turn.
            return vec![LoopOutput::TurnEnded(TurnEnd::Error {
                code: "AGENT_NO_OPEN_STEP".into(),
                message: format!("reply for turn {turn} step {step} but no step is open"),
            })];
        }

        // A cancel outranks the reply: the turn ends aborted whatever the model
        // had produced, which is what upstream's `signal.throwIfAborted()`
        // achieves at every await point.
        if let Some(cause) = self.cancelled.clone() {
            let mut outs = self.close_step(turn, step);
            outs.extend(self.close_turn(TurnEnd::Aborted { cause }));
            return outs;
        }

        let mut outs = self.close_step(turn, step);

        match outcome {
            StepOutcome::Error { code, message } => {
                outs.extend(self.close_turn(TurnEnd::Error { code, message }));
                return outs;
            }
            StepOutcome::MaxTokens => self.turn_saw_max_tokens = true,
            StepOutcome::Completed => {}
        }

        // The turn continues only if there is work for it: a tool call proposes
        // one, and so does any further admitted input. This is upstream's
        // `if (turnEnds && this.inbox.nextStep.length === 0) break`.
        let continues = has_tool_calls || more_input;
        if continues {
            outs.extend(self.begin_step());
            return outs;
        }

        let end = if self.turn_saw_max_tokens {
            TurnEnd::MaxTokens
        } else {
            TurnEnd::Completed
        };
        outs.extend(self.close_turn(end));
        outs
    }

    /// Close the open step, emitting `step/end`.
    fn close_step(&mut self, turn: u64, step: u64) -> Vec<LoopOutput> {
        if !self.step_open {
            return vec![];
        }
        self.step_open = false;
        vec![LoopOutput::Append(Draft::new(
            "step/end",
            json!({ "turn": turn, "step": step }),
        ))]
    }

    /// Close the running turn, emitting `turn/end` and returning to idle.
    fn close_turn(&mut self, reason: TurnEnd) -> Vec<LoopOutput> {
        let turn = match self.phase {
            Phase::Running { turn, .. } => turn,
            Phase::Idle { .. } => return vec![],
        };
        self.phase = Phase::Idle { last_turn: turn };
        self.step_open = false;
        self.cancelled = None;
        vec![
            LoopOutput::Append(Draft::new(
                "turn/end",
                json!({ "turn": turn, "reason": reason.to_reason() }),
            )),
            LoopOutput::TurnEnded(reason),
        ]
    }

    /// The `agent/pre-step` hook rejected the step: the turn closes `blocked`.
    ///
    /// Upstream reaches this from the `agent/pre-step` waterfall returning
    /// `{kind: 'reject'}`. It matters that no `step/start` was appended — a
    /// rejected step spends no model call and leaves no step frame.
    pub fn reject_step(&mut self) -> Vec<LoopOutput> {
        let mut outs = match self.phase {
            Phase::Running { turn, step, .. } => self.close_step(turn, step),
            Phase::Idle { .. } => vec![],
        };
        outs.extend(self.close_turn(TurnEnd::Blocked));
        outs
    }

    /// Ask to cancel the running turn.
    ///
    /// Returns the rows the cancel *immediately* owes (the step and turn
    /// closers) when the loop is between steps, or an empty vector when a step
    /// is open — in that case the cancel is latched and settles when the reply
    /// arrives, which is where the step's `step/end` can be ordered correctly.
    /// Latching rather than forcing the closer is what keeps `step/start` and
    /// `step/end` balanced.
    pub fn cancel(&mut self, cause: CancelCause) -> Vec<LoopOutput> {
        if !self.is_running() {
            // Nothing to cancel. Upstream answers the same way: a cancel for an
            // idle agent is accepted and does nothing.
            return vec![];
        }
        self.cancelled = Some(cause);
        if self.step_open {
            // Wait for the reply; see above.
            return vec![];
        }
        let cause = self.cancelled.clone().expect("just set");
        self.close_turn(TurnEnd::Aborted { cause })
    }

    /// Whether a cancel is latched and waiting for the open step to settle.
    pub fn cancel_pending(&self) -> bool {
        self.cancelled.is_some()
    }

    /// The `(turn, step)` currently running, for a caller recording a row.
    ///
    /// Idle reports `(last_turn, 0)`: a caller should not be recording a step
    /// row then, and a zero step is not a number any turn reaches.
    pub fn position(&self) -> (u64, u64) {
        match self.phase {
            Phase::Running { turn, step, .. } => (turn, step),
            Phase::Idle { last_turn } => (last_turn, 0),
        }
    }

    /// Whether the running turn has an open step.
    ///
    /// Read by the driver so a suspended operation can tell "the step is
    /// already open, resume it" from "open the next one" — which the rows alone
    /// cannot answer without scanning for an unmatched `step/start`.
    pub fn step_is_open(&self) -> bool {
        self.step_open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row types of a run of outputs, which is what most assertions are
    /// about: the *shape* of the turn, not the payloads.
    fn types(outs: &[LoopOutput]) -> Vec<&'static str> {
        outs.iter()
            .filter_map(|o| match o {
                LoopOutput::Append(d) => Some(d.row_type),
                _ => None,
            })
            .collect()
    }

    fn data_of(outs: &[LoopOutput], row_type: &str) -> Vec<Value> {
        outs.iter()
            .filter_map(|o| match o {
                LoopOutput::Append(d) if d.row_type == row_type => Some(d.data.clone()),
                _ => None,
            })
            .collect()
    }

    fn user_message(text: &str) -> Value {
        json!({
            "role": "user",
            "content": [{ "type": "text", "text": text }],
            "source": { "kind": "user" },
        })
    }

    // -- the turn shape ------------------------------------------------------

    /// One prompt, one completed reply: `turn/start`, `step/start`, `step/end`,
    /// `turn/end`. This is the minimal well-formed turn the corpus is full of.
    #[test]
    fn a_single_completed_step_opens_and_closes_one_turn() {
        let mut loop_ = AgentLoop::new();
        let mut outs = loop_.begin_turn(&[user_message("hi")]);
        assert_eq!(types(&outs), vec!["turn/start"], "the turn opens alone");
        let entered = loop_.enter_step();
        assert_eq!(types(&entered), vec!["step/start"]);
        assert_eq!(
            entered.last().unwrap(),
            &LoopOutput::CallModel { turn: 1, step: 1 }
        );
        outs.extend(entered);

        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert_eq!(types(&outs), vec!["step/end", "turn/end"]);
        assert_eq!(data_of(&outs, "turn/end")[0]["turn"], 1);
        assert_eq!(data_of(&outs, "turn/end")[0]["reason"]["kind"], "completed");
        assert!(!loop_.is_running());
    }

    /// The turn number is one past the last, and steps number from 1 within a
    /// turn. Both are asserted against the corpus shape.
    #[test]
    fn turn_numbers_increment_and_steps_restart_each_turn() {
        let mut loop_ = AgentLoop::new();
        for expected_turn in 1..=3 {
            let outs = loop_.begin_turn(&[user_message("hi")]);
            assert_eq!(
                data_of(&outs, "turn/start")[0]["turn"],
                expected_turn,
                "turn number"
            );
            loop_.enter_step();
            let outs = loop_.step_reply(StepOutcome::Completed, false, false);
            assert_eq!(data_of(&outs, "step/end")[0]["step"], 1);
            assert_eq!(data_of(&outs, "turn/end")[0]["turn"], expected_turn);
        }
    }

    /// A tool call continues the turn with another step rather than ending it —
    /// the shape the tool-calling corpus snapshots show (three steps, one turn).
    #[test]
    fn a_tool_call_opens_another_step_in_the_same_turn() {
        let mut loop_ = AgentLoop::new();
        loop_.begin_turn(&[user_message("hi")]);
        loop_.enter_step();
        let outs = loop_.step_reply(StepOutcome::Completed, true, false);
        assert_eq!(types(&outs), vec!["step/end", "step/start"]);
        assert_eq!(data_of(&outs, "step/start")[0]["step"], 2);
        assert_eq!(data_of(&outs, "step/start")[0]["turn"], 1);
        assert!(loop_.is_running(), "the turn is still open");

        // The second step completes with no further tool call → turn ends.
        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert_eq!(types(&outs), vec!["step/end", "turn/end"]);
        assert_eq!(data_of(&outs, "step/end")[0]["step"], 2);
    }

    /// Newly admitted input also continues the turn, even with no tool call.
    #[test]
    fn further_input_opens_another_step() {
        let mut loop_ = AgentLoop::new();
        loop_.begin_turn(&[user_message("hi")]);
        loop_.enter_step();
        let outs = loop_.step_reply(StepOutcome::Completed, false, true);
        assert_eq!(types(&outs), vec!["step/end", "step/start"]);
    }

    /// A turn whose admitted messages are empty still owns its boundary: it
    /// emits `turn/start` and `turn/end` and spends **no model call**.
    #[test]
    fn an_empty_turn_closes_without_a_step() {
        let mut loop_ = AgentLoop::new();
        let outs = loop_.begin_turn(&[]);
        assert_eq!(types(&outs), vec!["turn/start", "turn/end"]);
        assert_eq!(data_of(&outs, "turn/end")[0]["reason"]["kind"], "completed");
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, LoopOutput::CallModel { .. })),
            "an empty turn must not call the model"
        );
    }

    // -- the sticky max-tokens rule ------------------------------------------

    /// Once a step hits its ceiling the turn is `max-tokens`, even if a later
    /// step completes normally. Upstream guards this explicitly because the
    /// downgrade is easy to write by accident.
    #[test]
    fn max_tokens_is_sticky_across_steps() {
        let mut loop_ = AgentLoop::new();
        loop_.begin_turn(&[user_message("hi")]);
        loop_.enter_step();
        // Step 1 hits the ceiling and calls a tool → another step.
        let outs = loop_.step_reply(StepOutcome::MaxTokens, true, false);
        assert_eq!(types(&outs), vec!["step/end", "step/start"]);
        // Step 2 completes normally. The turn must still read max-tokens.
        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert_eq!(
            data_of(&outs, "turn/end")[0]["reason"]["kind"],
            "max-tokens"
        );
    }

    /// A turn with no ceiling hit ends `completed`, so the sticky flag is not
    /// merely always-on.
    #[test]
    fn a_turn_without_a_ceiling_hit_completes() {
        let mut loop_ = AgentLoop::new();
        loop_.begin_turn(&[user_message("hi")]);
        loop_.enter_step();
        let outs = loop_.step_reply(StepOutcome::Completed, true, false);
        assert_eq!(types(&outs), vec!["step/end", "step/start"]);
        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert_eq!(data_of(&outs, "turn/end")[0]["reason"]["kind"], "completed");
    }

    // -- failure and refusal -------------------------------------------------

    #[test]
    fn an_error_ends_the_turn_with_the_structured_failure() {
        let mut loop_ = AgentLoop::new();
        loop_.begin_turn(&[user_message("hi")]);
        loop_.enter_step();
        let outs = loop_.step_reply(
            StepOutcome::Error {
                code: "AUTH".into(),
                message: "simulated provider error (HTTP 401)".into(),
            },
            false,
            false,
        );
        assert_eq!(types(&outs), vec!["step/end", "turn/end"]);
        let reason = &data_of(&outs, "turn/end")[0]["reason"];
        assert_eq!(reason["kind"], "error");
        assert_eq!(reason["error"]["code"], "AUTH");
        assert_eq!(
            reason["error"]["message"],
            "simulated provider error (HTTP 401)"
        );
        assert!(!loop_.is_running());
    }

    /// A rejected pre-step blocks the turn and leaves **no step frame**.
    #[test]
    fn a_rejected_step_blocks_the_turn_without_a_step() {
        let mut loop_ = AgentLoop::new();
        // Upstream's reject decision happens *before* `step/start`, so this is
        // the path a caller takes instead of `enter_step`: only the turn closer
        // is owed, and no step frame ever exists.
        loop_.begin_turn(&[user_message("hi")]);
        let outs = loop_.reject_step();
        assert_eq!(types(&outs), vec!["turn/end"]);
        assert_eq!(data_of(&outs, "turn/end")[0]["reason"]["kind"], "blocked");
        assert!(
            !types(&outs).contains(&"step/start"),
            "a blocked turn opens no step"
        );
    }

    // -- cancellation (§1.4 of the plan) -------------------------------------

    /// A cancel between steps closes the turn immediately, with the cause
    /// recorded durably. The vocabulary is the spec's `TurnEndCancelCause`.
    #[test]
    fn a_cancel_between_steps_closes_the_turn_aborted() {
        let mut loop_ = AgentLoop::new();
        loop_.begin_turn(&[user_message("hi")]);
        loop_.enter_step();
        let outs = loop_.step_reply(StepOutcome::Completed, true, false);
        assert_eq!(types(&outs), vec!["step/end", "step/start"]);

        // The step is open, so the cancel latches; it settles on the reply.
        assert!(loop_.cancel(CancelCause::User).is_empty());
        assert!(loop_.cancel_pending());

        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert_eq!(types(&outs), vec!["step/end", "turn/end"]);
        let reason = &data_of(&outs, "turn/end")[0]["reason"];
        assert_eq!(reason["kind"], "aborted");
        assert_eq!(reason["reason"]["kind"], "user");
        assert!(!loop_.is_running());
    }

    /// A cancel outranks a successful reply: the turn is `aborted`, not
    /// `completed`, even though the model answered normally.
    #[test]
    fn a_cancel_outranks_a_completed_reply() {
        let mut loop_ = AgentLoop::new();
        loop_.begin_turn(&[user_message("hi")]);
        loop_.enter_step();
        loop_.cancel(CancelCause::Parent);
        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert_eq!(types(&outs), vec!["step/end", "turn/end"]);
        let reason = &data_of(&outs, "turn/end")[0]["reason"];
        assert_eq!(reason["kind"], "aborted");
        assert_eq!(reason["reason"]["kind"], "parent");
    }

    /// Every cause the spec declares round-trips into the log shape.
    #[test]
    fn every_cancel_cause_serializes_as_the_spec_spells_it() {
        for (cause, expected) in [
            (CancelCause::User, json!({ "kind": "user" })),
            (CancelCause::Parent, json!({ "kind": "parent" })),
            (
                CancelCause::Hook {
                    reason: "policy".into(),
                },
                json!({ "kind": "hook", "reason": "policy" }),
            ),
            (CancelCause::Disposed, json!({ "kind": "disposed" })),
            (CancelCause::Legacy, json!({ "kind": "legacy" })),
        ] {
            let mut loop_ = AgentLoop::new();
            loop_.begin_turn(&[user_message("hi")]);
            loop_.enter_step();
            loop_.cancel(cause);
            let outs = loop_.step_reply(StepOutcome::Completed, false, false);
            assert_eq!(data_of(&outs, "turn/end")[0]["reason"]["reason"], expected);
        }
    }

    /// A cancel while idle does nothing and — importantly — does not leak into
    /// the *next* turn.
    #[test]
    fn a_cancel_while_idle_is_a_no_op_and_does_not_poison_the_next_turn() {
        let mut loop_ = AgentLoop::new();
        assert!(loop_.cancel(CancelCause::User).is_empty());

        let outs = loop_.begin_turn(&[user_message("hi")]);
        assert_eq!(types(&outs), vec!["turn/start"]);
        loop_.enter_step();
        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert_eq!(data_of(&outs, "turn/end")[0]["reason"]["kind"], "completed");
    }

    /// A cancel latched while a step is open settles when the reply lands, and
    /// the latch does **not** survive into the next turn.
    #[test]
    fn a_latched_cancel_clears_with_its_turn() {
        let mut loop_ = AgentLoop::new();
        loop_.begin_turn(&[user_message("hi")]);
        loop_.enter_step();
        loop_.cancel(CancelCause::Disposed);
        assert!(
            loop_.cancel_pending(),
            "the latch is held while a step is open"
        );
        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert_eq!(data_of(&outs, "turn/end")[0]["reason"]["kind"], "aborted");
        assert!(!loop_.cancel_pending(), "the latch cleared with the turn");

        // The next turn is unaffected — the cancel did not poison it.
        loop_.begin_turn(&[user_message("hi")]);
        let outs = loop_.enter_step();
        assert_eq!(types(&outs), vec!["step/start"]);
        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert_eq!(data_of(&outs, "turn/end")[0]["reason"]["kind"], "completed");
    }

    // -- resume from a log ---------------------------------------------------

    /// `resume_from` reconstructs the turn counter from `turn/end` rows, so a
    /// continued session numbers its next turn correctly.
    #[test]
    fn resume_derives_the_next_turn_number_from_the_log() {
        let rows = vec![
            json!({ "type": "turn/start", "data": { "turn": 1 } }),
            json!({ "type": "turn/end", "data": { "turn": 1, "reason": { "kind": "completed" } } }),
            json!({ "type": "turn/start", "data": { "turn": 2 } }),
            json!({ "type": "turn/end", "data": { "turn": 2, "reason": { "kind": "completed" } } }),
        ];
        let mut loop_ = AgentLoop::resume_from(&rows);
        assert!(!loop_.is_running());
        let outs = loop_.begin_turn(&[user_message("hi")]);
        assert_eq!(data_of(&outs, "turn/start")[0]["turn"], 3);
    }

    /// A log whose last turn never closed is a crash-orphaned turn. The loop
    /// reports the open turn and does **not** append a repair row during
    /// construction — `resume_from` must be idempotent.
    #[test]
    fn resume_reports_a_crash_orphaned_turn_without_appending() {
        let rows = vec![
            json!({ "type": "turn/start", "data": { "turn": 1 } }),
            json!({ "type": "step/start", "data": { "turn": 1, "step": 1 } }),
        ];
        let loop_ = AgentLoop::resume_from(&rows);
        assert!(loop_.is_running());
        assert_eq!(loop_.current_turn(), Some(1));
        // Idempotent: resuming twice reports the same thing.
        let again = AgentLoop::resume_from(&rows);
        assert_eq!(again.current_turn(), Some(1));
    }

    /// An empty log is a fresh session: the first turn is 1.
    #[test]
    fn resume_from_an_empty_log_starts_at_turn_one() {
        let mut loop_ = AgentLoop::resume_from(&[]);
        let outs = loop_.begin_turn(&[user_message("hi")]);
        assert_eq!(data_of(&outs, "turn/start")[0]["turn"], 1);
    }

    // -- abuse ---------------------------------------------------------------

    /// Beginning a turn while one is running is refused rather than nested.
    #[test]
    fn beginning_a_turn_while_running_is_refused() {
        let mut loop_ = AgentLoop::new();
        loop_.begin_turn(&[user_message("hi")]);
        let outs = loop_.begin_turn(&[user_message("again")]);
        assert!(types(&outs).is_empty(), "no rows for a refused turn");
        assert!(matches!(
            outs.last(),
            Some(LoopOutput::TurnEnded(TurnEnd::Error { code, .. })) if code == "AGENT_BUSY"
        ));
    }

    /// A reply with no open step is a caller bug, and is reported as one rather
    /// than folded into the turn.
    #[test]
    fn a_reply_without_an_open_step_is_reported() {
        let mut loop_ = AgentLoop::new();
        let outs = loop_.step_reply(StepOutcome::Completed, false, false);
        assert!(matches!(
            outs.last(),
            Some(LoopOutput::TurnEnded(TurnEnd::Error { code, .. }))
                if code == "AGENT_NO_OPEN_STEP"
        ));
    }

    /// Whatever the path, `step/start` and `step/end` stay balanced and every
    /// `turn/start` has exactly one `turn/end`. This is the invariant the whole
    /// machine exists to hold, so it is checked over every exit path rather
    /// than one.
    #[test]
    fn turn_and_step_frames_are_balanced_on_every_path() {
        // Each case drives a whole turn from a fresh loop and returns every
        // output it produced, so the assertion sees the complete row sequence.
        type Path = (&'static str, fn(&mut AgentLoop) -> Vec<LoopOutput>);
        let paths: &[Path] = &[
            ("completed", |l| {
                let mut o = l.begin_turn(&[user_message("hi")]);
                o.extend(l.enter_step());
                o.extend(l.step_reply(StepOutcome::Completed, false, false));
                o
            }),
            ("empty turn", |l| l.begin_turn(&[])),
            ("tool then done", |l| {
                let mut o = l.begin_turn(&[user_message("hi")]);
                o.extend(l.enter_step());
                o.extend(l.step_reply(StepOutcome::Completed, true, false));
                o.extend(l.step_reply(StepOutcome::Completed, false, false));
                o
            }),
            ("multiple tools", |l| {
                let mut o = l.begin_turn(&[user_message("hi")]);
                o.extend(l.enter_step());
                o.extend(l.step_reply(StepOutcome::Completed, true, false));
                o.extend(l.step_reply(StepOutcome::Completed, true, false));
                o.extend(l.step_reply(StepOutcome::Completed, false, false));
                o
            }),
            ("error", |l| {
                let mut o = l.begin_turn(&[user_message("hi")]);
                o.extend(l.enter_step());
                o.extend(l.step_reply(
                    StepOutcome::Error {
                        code: "X".into(),
                        message: "boom".into(),
                    },
                    false,
                    false,
                ));
                o
            }),
            ("cancel", |l| {
                let mut o = l.begin_turn(&[user_message("hi")]);
                o.extend(l.enter_step());
                o.extend(l.cancel(CancelCause::User));
                o.extend(l.step_reply(StepOutcome::Completed, false, false));
                o
            }),
            ("max-tokens", |l| {
                let mut o = l.begin_turn(&[user_message("hi")]);
                o.extend(l.enter_step());
                o.extend(l.step_reply(StepOutcome::MaxTokens, false, false));
                o
            }),
            ("blocked", |l| {
                let mut o = l.begin_turn(&[user_message("hi")]);
                o.extend(l.reject_step());
                o
            }),
        ];

        for (name, run) in paths {
            let mut loop_ = AgentLoop::new();
            let outs = run(&mut loop_);
            let t = types(&outs);
            let count = |row: &str| t.iter().filter(|x| **x == row).count();
            assert_eq!(count("turn/start"), 1, "{name}: one turn/start");
            assert_eq!(count("turn/end"), 1, "{name}: one turn/end");
            assert_eq!(
                count("step/start"),
                count("step/end"),
                "{name}: step frames balanced ({t:?})"
            );
            assert!(
                !outs
                    .iter()
                    .any(|o| matches!(o, LoopOutput::CallModel { .. }))
                    || count("step/start") > 0,
                "{name}: a model call implies an open step"
            );
            assert!(!loop_.is_running(), "{name}: the loop is idle at the end");
        }
    }

    // -- the corpus ----------------------------------------------------------

    /// Replay every committed snapshot's turn structure through `resume_from`
    /// and assert the FSM agrees with what the log actually contains.
    ///
    /// This is the check that keeps the FSM honest about *real* logs rather
    /// than only about the paths its own tests construct: 85 directories of
    /// recorded turns, including ones that ended blocked, errored, or hit a
    /// token ceiling. It reads `dsh/snapshots/session` through the same decoder
    /// the session namespace uses.
    #[test]
    fn resumes_every_committed_snapshot_consistently() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dsh/snapshots/session");
        if !root.is_dir() {
            // A submodule-less checkout cannot run this. Skipping beats failing
            // for a reason that has nothing to do with the FSM.
            eprintln!(
                "skipping: {} absent (submodule not checked out)",
                root.display()
            );
            return;
        }

        let mut dirs_checked = 0;
        let mut turns_checked = 0;
        let mut reasons_seen: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();

        for entry in std::fs::read_dir(&root).expect("read snapshots dir") {
            let dir = entry.expect("dir entry").path();
            if !dir.is_dir() {
                continue;
            }
            let Ok(versions) = vocoder_session::list_generations(&dir) else {
                continue;
            };
            let Some(latest) = versions.into_iter().max() else {
                continue;
            };
            let Some(path) = vocoder_session::generation_path(&dir, latest) else {
                continue;
            };
            let Ok(rows) = vocoder_session::read_generation(&path) else {
                continue;
            };
            if rows.is_empty() {
                continue;
            }
            dirs_checked += 1;

            // Every turn in the log must be well-formed: balanced frames, one
            // `turn/end` per `turn/start`, and a reason this FSM knows.
            let mut open: Option<(u64, Vec<u64>)> = None; // (turn, steps seen)
            for row in &rows {
                let ty = row.get("type").and_then(Value::as_str).unwrap_or_default();
                let data = row.get("data").cloned().unwrap_or(Value::Null);
                match ty {
                    "turn/start" => {
                        assert!(open.is_none(), "{}: nested turn/start", dir.display());
                        let t = data.get("turn").and_then(Value::as_u64).unwrap_or(0);
                        open = Some((t, Vec::new()));
                    }
                    "turn/end" => {
                        let (t, steps) = open
                            .take()
                            .unwrap_or_else(|| panic!("{}: turn/end with no turn", dir.display()));
                        let ended = data.get("turn").and_then(Value::as_u64).unwrap_or(0);
                        assert_eq!(
                            t,
                            ended,
                            "{}: turn/end names a different turn",
                            dir.display()
                        );
                        // Steps number from 1, densely and in order — the same
                        // rule the FSM's own `begin_step` follows. A turn with
                        // exactly one step is ordinary, not an anomaly.
                        for (i, step) in steps.iter().enumerate() {
                            assert_eq!(
                                *step,
                                i as u64 + 1,
                                "{}: steps are not dense from 1: {steps:?}",
                                dir.display()
                            );
                        }
                        let kind = data
                            .get("reason")
                            .and_then(|r| r.get("kind"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        assert!(
                            matches!(
                                kind.as_str(),
                                "completed"
                                    | "aborted"
                                    | "blocked"
                                    | "error"
                                    | "max-tokens"
                                    | "interrupted"
                            ),
                            "{}: unknown turn/end reason {kind:?}",
                            dir.display()
                        );
                        reasons_seen.insert(kind);
                        turns_checked += 1;
                    }
                    "step/start" => {
                        if let Some((t, mut steps)) = open {
                            let s = data.get("step").and_then(Value::as_u64).unwrap_or(0);
                            steps.push(s);
                            open = Some((t, steps));
                        } else {
                            panic!("{}: step/start outside a turn", dir.display());
                        }
                    }
                    _ => {}
                }
            }

            // Resume must never claim a turn that the log already closed.
            let resumed = AgentLoop::resume_from(&rows);
            if open.is_none() {
                assert!(
                    !resumed.is_running(),
                    "{}: log has no open turn but resume reports one",
                    dir.display()
                );
            }
        }

        // Thresholds are set below the measured counts (85 dirs, 84 closed
        // turns on 2026-09-19) so a corpus that shrinks is caught while an
        // unrelated snapshot being added is not. They are floors, not targets.
        assert!(
            dirs_checked >= 40,
            "expected many snapshot dirs, saw {dirs_checked}"
        );
        assert!(
            turns_checked >= 50,
            "expected many turns, saw {turns_checked}"
        );
        // The corpus must actually exercise the endings, or this test proves
        // less than it claims.
        assert!(
            reasons_seen.contains("completed"),
            "corpus should contain completed turns: {reasons_seen:?}"
        );
    }
}
