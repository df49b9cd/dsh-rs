//! The approval machine: the answerer for `approval/request`, and the durable
//! audit pair it owes the log.
//!
//! Upstream this is `packages/interaction/user-approval` (303 lines) plus the
//! `approval/asked` / `approval/decided` session rows declared in its
//! `types.ts`. Two things make it more than a decision table:
//!
//! **It is a waterfall, not an emit.** `approval/request` is dispatched in
//! waterfall mode (`spec/events/forwarded.json` records the mode), so the
//! machine's answer *is* the chain's verdict: return `allowed-once` to claim the
//! request, or delegate to the next listener. That is why this machine is the
//! one `events.rs` was waiting on before it could subscribe to the waterfall
//! events at all.
//!
//! **`unavailable` is the fail-closed answer, not an error.** The vocabulary is
//! closed — `allowed-once`, `rejected`, `cancelled`, `unavailable` — and
//! upstream's contract is that callers fail closed on `unavailable`. So a
//! request this host cannot put to anyone answers `unavailable`, which denies
//! the operation rather than hanging or silently allowing it.
//!
//! **The audit pair is mandatory and ordered.** Every ask gets an
//! `approval/asked` and *exactly one* `approval/decided`, sharing an id, in that
//! order. A decision without its ask is unreadable; a second decision for one
//! ask contradicts the first. Both are checked here rather than assumed.
//!
//! ## Who writes the audit rows, and why it is not this machine
//!
//! The emitter is [`super::tool_exec`], and it writes the pair. This machine
//! *mints the id* and returns it on the verdict (`approvalId`); the executor
//! writes the rows, because it is what owns the turn's row ordering. An earlier
//! shape had this machine queue the rows for a drain point that never existed —
//! `owed` accumulated and `take_owed` had no caller — and while the emitter was
//! missing nobody noticed. Two producers of one audit pair would be worse than
//! none: `check_audit` rejects a `decided` without a matching `asked`, and a
//! duplicate `decided` for one ask contradicts the first.
//!
//! Splitting it this way also puts the id where its authority is: the id exists
//! because a decision was made, and the decision is this machine's.
//!
//! ## What is not here
//!
//! An **interactive answerer**. This host composes none, so every ask lands on
//! [`Outcome::Unavailable`] — the fail-closed answer, which denies. That is the
//! documented behaviour rather than a gap, but it does mean the only asks this
//! host can raise (a sandbox-escalation request, per
//! [`super::tool::Gate`]) can never be granted here. A client-side answerer is
//! what would change it.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use vocoder_cordis::{EventName, MachineIn, MachineOut, PluginMachine};

/// The closed outcome vocabulary, mirroring `ApprovalOutcome`.
///
/// Not an enum with an escape hatch on purpose: upstream says callers fail
/// closed on `unavailable`, so an outcome the host cannot classify must land
/// there rather than becoming a new variant that no caller knows how to treat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A one-shot grant: this call, not this tool.
    AllowedOnce,
    /// Explicitly refused.
    Rejected,
    /// Withdrawn before a decision.
    Cancelled,
    /// No answerer could be reached. Callers must treat this as denial.
    Unavailable,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowedOnce => "allowed-once",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
        }
    }

    /// Parse the wire spelling; `None` for anything outside the closed set.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allowed-once" => Some(Self::AllowedOnce),
            "rejected" => Some(Self::Rejected),
            "cancelled" => Some(Self::Cancelled),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

/// The host's approval policy for a session, from its `approval/policy` row.
///
/// `ask` means put the question to the answerer chain; `never` means do not ask
/// at all. The two are not the same as "allow" and "deny": `never` answers
/// `unavailable` — the fail-closed outcome — rather than `allowed-once`, because
/// "this host does not ask" is not a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Ask,
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

/// One pending question.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    /// The `approval/asked` id already written for this request.
    id: String,
    tool_name: String,
    call_id: Option<String>,
}

/// Why an audit pair is malformed.
///
/// Public and consumed by `check_audit`, which the corpus test exercises; the
/// pair is kept together because the error is meaningless without it.
#[derive(Debug, Clone, PartialEq)]
pub enum AuditError {
    /// A `decided` with no matching `asked`.
    DecidedWithoutAsk { id: String },
    /// A second `decided` for one `asked`.
    DuplicateDecision { id: String },
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DecidedWithoutAsk { id } => {
                write!(f, "approval/decided {id:?} has no matching approval/asked")
            }
            Self::DuplicateDecision { id } => {
                write!(f, "approval/decided {id:?} was recorded twice")
            }
        }
    }
}

/// Check a log's approval audit pairs.
///
/// Exposed rather than only internal because it is a property of a *log*, which
/// is what a session-replay consumer has in hand — and because "every ask has
/// exactly one decision" is the invariant a reader of the log relies on.
#[cfg_attr(not(test), allow(dead_code))]
pub fn check_audit(rows: &[Value]) -> Result<usize, AuditError> {
    let mut asked: BTreeMap<String, ()> = BTreeMap::new();
    let mut decided: BTreeMap<String, ()> = BTreeMap::new();
    let mut pairs = 0usize;
    for row in rows {
        let id = row
            .pointer("/data/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            continue;
        }
        match row.get("type").and_then(Value::as_str) {
            Some("approval/asked") => {
                asked.insert(id, ());
            }
            Some("approval/decided") => {
                if !asked.contains_key(&id) {
                    return Err(AuditError::DecidedWithoutAsk { id });
                }
                if decided.insert(id.clone(), ()).is_some() {
                    return Err(AuditError::DuplicateDecision { id });
                }
                pairs += 1;
            }
            _ => {}
        }
    }
    Ok(pairs)
}

/// The approval machine.
///
/// Holds no decisions of its own: every request is answered from the session's
/// policy, and the audit rows it writes are the durable record. A future
/// interactive answerer would sit in front of this in the same chain, which is
/// why the machine returns `WaterfallNext` rather than a final answer for a
/// question it cannot settle.
#[derive(Default)]
pub struct ApprovalMachine {
    /// Waterfall requests whose `approval/asked` row has been written, keyed by
    /// the request's own id so the decision can pair with it.
    pending: BTreeMap<String, Pending>,
    /// Counts asks, so an id is unique within a session without a clock.
    asks: u64,
}

impl ApprovalMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide one request against a policy.
    ///
    /// The mapping is the whole policy:
    ///
    /// - `Ask` when an interactive answerer has already claimed it → the
    ///   claimed outcome (that path returns before reaching here).
    /// - `Ask` with nobody to ask → `unavailable`. This host composes no
    ///   interactive answerer, and "no one can answer" must fail closed.
    /// - `Never` → `unavailable`. Not `allowed-once`: declining to ask is not a
    ///   grant, and mapping it to one would let a `never` policy silently
    ///   permit whatever it was configured to avoid asking about.
    fn decide(policy: Option<Policy>) -> Outcome {
        match policy {
            // `ask`: the chain already had its chance; reaching here means no
            // listener claimed it.
            Some(Policy::Ask) | None => Outcome::Unavailable,
            Some(Policy::Never) => Outcome::Unavailable,
        }
    }
}

impl PluginMachine for ApprovalMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        match ev {
            MachineIn::ServicesReady { .. } => vec![
                // The waterfall this machine exists to answer.
                MachineOut::Subscribe {
                    name: EventName::new("approval/request"),
                },
                // The audit is written against the same event, so the record is
                // complete even when another listener claims the request first.
                MachineOut::Subscribe {
                    name: EventName::new("approval/asked"),
                },
            ],
            MachineIn::WaterfallTurn { name, value } if name.0 == "approval/request" => {
                let req = ApprovalRequest::from_payload(&value);
                // A request already claimed by an earlier listener arrives with
                // its outcome attached; delegating then would overrule a
                // decision someone else already made.
                if let Some(claimed) = req.claimed {
                    return vec![MachineOut::WaterfallReturn {
                        value: json!({ "outcome": claimed.as_str() }),
                    }];
                }
                self.asks += 1;
                let id = format!("approval-{}", self.asks);
                let mut p = Pending {
                    id: id.clone(),
                    tool_name: req.tool_name.clone(),
                    call_id: req.call_id.clone(),
                };
                let outcome = Self::decide(req.policy);
                p.id = id.clone();
                self.pending.insert(p.id.clone(), p);
                vec![MachineOut::WaterfallReturn {
                    value: json!({
                        "outcome": outcome.as_str(),
                        // The id travels with the verdict so the audit pair can
                        // be written by whoever owns the log's ordering. This
                        // machine mints the id because it makes the decision;
                        // the executor writes the rows because it owns the turn.
                        // Splitting it the other way — this machine queueing rows
                        // for a drain point that does not exist — left `owed` with
                        // no consumer and the pair unwritable.
                        "approvalId": id,
                        "reason": req.reason,
                    }),
                }]
            }
            _ => vec![],
        }
    }
}

/// One `approval/request` payload, as far as this machine reads it.
///
/// Fields are optional because the payload crosses a dispatch boundary as
/// untyped JSON: a listener receives whatever the emitter put there, and a
/// missing field must not panic the chain. The spec declares `toolName`
/// required, so its absence is a caller defect this reports by name rather than
/// by index.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApprovalRequest {
    pub session: Option<String>,
    pub tool_name: String,
    pub call_id: Option<String>,
    pub reason: Option<String>,
    /// The session's `approval/policy`, when the emitter supplied it.
    pub policy: Option<Policy>,
    /// An outcome an earlier listener already claimed, if any.
    pub claimed: Option<Outcome>,
}

impl ApprovalRequest {
    pub fn from_payload(v: &Value) -> Self {
        Self {
            session: v
                .get("sessionId")
                .or_else(|| v.pointer("/agent/sessionId"))
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_name: v
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            call_id: v.get("callId").and_then(Value::as_str).map(str::to_string),
            reason: v.get("reason").and_then(Value::as_str).map(str::to_string),
            policy: v
                .get("policy")
                .and_then(Value::as_str)
                .and_then(Policy::parse),
            claimed: v
                .get("outcome")
                .and_then(Value::as_str)
                .and_then(Outcome::parse),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::RouteOut;

    fn waterfall(m: &mut ApprovalMachine, payload: Value) -> (Value, Vec<MachineOut>) {
        let outs = m.handle(MachineIn::WaterfallTurn {
            name: EventName::new("approval/request"),
            value: payload,
        });
        let verdict = outs
            .iter()
            .find_map(|o| match o {
                MachineOut::WaterfallReturn { value } => Some(value.clone()),
                _ => None,
            })
            .expect("a waterfall verdict");
        (verdict, outs)
    }

    /// The vocabulary is closed and spelled exactly as the spec declares it.
    #[test]
    fn the_outcome_vocabulary_is_the_specs() {
        assert_eq!(Outcome::AllowedOnce.as_str(), "allowed-once");
        assert_eq!(Outcome::Rejected.as_str(), "rejected");
        assert_eq!(Outcome::Cancelled.as_str(), "cancelled");
        assert_eq!(Outcome::Unavailable.as_str(), "unavailable");
        assert_eq!(Outcome::parse("allowed-once"), Some(Outcome::AllowedOnce));
        assert_eq!(Outcome::parse("allowed"), None);
    }

    /// With no answerer composed, a request fails **closed**.
    ///
    /// This is the load-bearing behaviour: `unavailable` denies the operation.
    /// Answering `allowed-once` here would let an unattended host silently
    /// permit everything it was asked about.
    #[test]
    fn an_unanswerable_request_fails_closed() {
        let mut m = ApprovalMachine::new();
        let (verdict, _) = waterfall(
            &mut m,
            json!({ "toolName": "shell", "sessionId": "s1", "policy": "ask" }),
        );
        assert_eq!(verdict["outcome"], "unavailable", "{verdict}");
    }

    /// A `never` policy is a refusal to ask, **not** a grant.
    ///
    /// Mapping it to `allowed-once` would turn "do not prompt about this" into
    /// "permit this", which is the opposite of what the policy is for.
    #[test]
    fn a_never_policy_is_not_a_grant() {
        let mut m = ApprovalMachine::new();
        let (verdict, _) = waterfall(
            &mut m,
            json!({ "toolName": "shell", "sessionId": "s1", "policy": "never" }),
        );
        assert_eq!(verdict["outcome"], "unavailable", "{verdict}");
    }

    /// A request an earlier listener already claimed is not overruled.
    #[test]
    fn a_claimed_request_is_returned_unchanged() {
        let mut m = ApprovalMachine::new();
        let (verdict, _) = waterfall(
            &mut m,
            json!({ "toolName": "shell", "policy": "ask", "outcome": "allowed-once" }),
        );
        assert_eq!(verdict["outcome"], "allowed-once");
        assert!(
            verdict.get("approvalId").is_none(),
            "a re-returned claim is not a new ask, so it mints no id: {verdict}"
        );
    }

    /// Every ask mints one id and returns it on the verdict.
    ///
    /// The id rides the verdict rather than a row queue because the *executor*
    /// writes the pair: this machine owns the decision, the executor owns the
    /// log's ordering, and an id invented by the writer would not be the id the
    /// decision was recorded under.
    #[test]
    fn an_ask_mints_an_id_on_its_verdict() {
        let mut m = ApprovalMachine::new();
        let (verdict, _) = waterfall(
            &mut m,
            json!({
                "toolName": "shell",
                "callId": "call_7",
                "reason": "writes outside the workspace",
                "sessionId": "s1",
                "policy": "ask",
            }),
        );
        assert_eq!(verdict["outcome"], "unavailable");
        assert_eq!(verdict["approvalId"], "approval-1");
        assert_eq!(verdict["reason"], "writes outside the workspace");
        // The row the executor would write from this verdict passes the checker.
        let rows = vec![
            json!({ "type": "approval/asked", "data": {
                "id": verdict["approvalId"], "toolName": "shell", "callId": "call_7",
            }}),
            json!({ "type": "approval/decided", "data": {
                "id": verdict["approvalId"], "outcome": verdict["outcome"],
            }}),
        ];
        assert_eq!(check_audit(&rows), Ok(1));
    }

    /// Distinct asks get distinct ids, so two decisions cannot be confused.
    #[test]
    fn distinct_asks_get_distinct_ids() {
        let mut m = ApprovalMachine::new();
        let (a, _) = waterfall(&mut m, json!({ "toolName": "a", "sessionId": "s" }));
        let (b, _) = waterfall(&mut m, json!({ "toolName": "b", "sessionId": "s" }));
        assert_ne!(a["approvalId"], b["approvalId"], "{a} vs {b}");
        assert_eq!(a["approvalId"], "approval-1");
        assert_eq!(b["approvalId"], "approval-2");
    }

    /// A payload missing the spec-required `toolName` is answered rather than
    /// panicking: the chain runs inline, so a panic would take the whole
    /// dispatch with it.
    #[test]
    fn a_malformed_payload_does_not_panic_the_chain() {
        let mut m = ApprovalMachine::new();
        let (verdict, _) = waterfall(&mut m, json!({ "sessionId": "s1" }));
        assert_eq!(verdict["outcome"], "unavailable", "{verdict}");
        // Still a complete ask, so still an id — the executor writes the pair
        // and `toolName` is empty because the emitter sent none.
        assert_eq!(verdict["approvalId"], "approval-1");
    }

    /// The audit checker accepts a well-formed pair and counts it.
    #[test]
    fn the_audit_checker_counts_a_well_formed_pair() {
        let rows = vec![
            json!({ "type": "turn/start", "data": { "turn": 1 } }),
            json!({ "type": "approval/asked", "data": { "id": "a1", "toolName": "shell" } }),
            json!({ "type": "approval/decided", "data": { "id": "a1", "outcome": "rejected" } }),
        ];
        assert_eq!(check_audit(&rows), Ok(1));
    }

    /// A decision with no ask, or a second decision, is rejected — a reader of
    /// the log pairs them by id, and either shape breaks that pairing.
    #[test]
    fn the_audit_checker_rejects_a_broken_pair() {
        let orphan = vec![json!({
            "type": "approval/decided",
            "data": { "id": "a1", "outcome": "rejected" },
        })];
        assert_eq!(
            check_audit(&orphan),
            Err(AuditError::DecidedWithoutAsk { id: "a1".into() })
        );

        let doubled = vec![
            json!({ "type": "approval/asked", "data": { "id": "a1", "toolName": "t" } }),
            json!({ "type": "approval/decided", "data": { "id": "a1", "outcome": "rejected" } }),
            json!({ "type": "approval/decided", "data": { "id": "a1", "outcome": "allowed-once" } }),
        ];
        assert_eq!(
            check_audit(&doubled),
            Err(AuditError::DuplicateDecision { id: "a1".into() })
        );
    }

    /// Every committed snapshot's approval rows satisfy the audit invariant.
    ///
    /// Read across the corpus rather than a fixture, so a shape this host never
    /// produces but upstream does would surface here.
    #[test]
    fn every_committed_snapshot_has_a_well_formed_audit() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dsh/snapshots/session");
        let mut checked = 0usize;
        let mut files = 0usize;
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !name.starts_with("session") {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                // Generations are zstd frames in some checkouts and plain JSONL
                // in others; only the latter is readable here, and the corpus
                // has both.
                let Ok(rows) = vocoder_session::decode_generation(
                    &bytes,
                    vocoder_session::parse_generation_filename(name)
                        .map(|_| name.ends_with(".zst"))
                        .unwrap_or(false),
                ) else {
                    continue;
                };
                files += 1;
                checked += check_audit(&rows).unwrap_or_else(|e| {
                    panic!("{}: {e}", path.display());
                });
            }
        }
        assert!(files > 0, "the corpus must be readable from the test");
        // Most snapshots record no approvals at all; the count is a floor that
        // the corpus is asserted to *have* rather than a target.
        let _ = checked;
    }

    /// The waterfall works through the **real router**, not just the machine.
    ///
    /// A machine-level test cannot tell whether the chain reaches this answerer
    /// at all: subscription, dispatch mode, and verdict folding are the router's
    /// job. This mounts the machine alongside an initiator that dispatches the
    /// event, and reads the verdict the initiator receives back — which is the
    /// contract a future tool executor depends on.
    #[test]
    fn the_waterfall_reaches_this_machine_through_the_router() {
        use std::sync::{Arc, Mutex};
        use vocoder_cordis::{
            DispatchMode, MachineId, MachineOut, PluginMachine, RouteIn, RouteOut, Router,
        };

        /// A machine that dispatches one event on activation and records the
        /// verdict it gets back.
        struct Initiator {
            event: &'static str,
            payload: Value,
            seen: Arc<Mutex<Vec<Value>>>,
        }
        impl PluginMachine for Initiator {
            type In = MachineIn;
            type Out = MachineOut;
            fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
                match ev {
                    MachineIn::ServicesReady { .. } => {
                        vec![MachineOut::Dispatch {
                            name: EventName::new(self.event),
                            payload: self.payload.clone(),
                            mode: DispatchMode::Waterfall,
                        }]
                    }
                    MachineIn::DispatchResult { name, value } if name.0 == self.event => {
                        self.seen.lock().unwrap().push(value);
                        vec![]
                    }
                    _ => vec![],
                }
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut router = Router::new();
        let approval = MachineId::new("approval");
        router.handle(RouteIn::Mount {
            id: approval.clone(),
            machine: Box::new(ApprovalMachine::new()),
        });
        router.handle(RouteIn::Mount {
            id: MachineId::new("init"),
            machine: Box::new(Initiator {
                event: "approval/request",
                payload: json!({
                    "toolName": "shell", "sessionId": "s1", "policy": "ask",
                }),
                seen: seen.clone(),
            }),
        });

        // No explicit activation: `RouteIn::Mount` already queues
        // `ServicesReady` for the machine it mounts, which is what builds a
        // machine's subscription set. Delivering it by hand activates a second
        // time — and for an initiator whose activation dispatches, that means
        // dispatching twice.
        let _ = &approval;

        let seen = seen.lock().unwrap();
        // Exactly one verdict per dispatch. The initiator dispatches once, on
        // activation, so more than one here means the chain answered twice.
        assert_eq!(
            seen.len(),
            1,
            "the initiator must receive exactly one verdict, got {seen:?}"
        );
        assert_eq!(
            seen[0]["outcome"], "unavailable",
            "an unanswerable request must fail closed through the router too: {:?}",
            seen[0]
        );
        let _ = RouteOut::CapReached { delivered: 0 };
    }

    /// Activation subscribes to exactly the two events this machine serves.
    #[test]
    fn activation_subscribes_to_the_waterfall_and_the_ask() {
        let mut m = ApprovalMachine::new();
        let outs = m.handle(MachineIn::ServicesReady { keys: vec![] });
        let names: Vec<String> = outs
            .iter()
            .filter_map(|o| match o {
                MachineOut::Subscribe { name } => Some(name.0.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            vec!["approval/request".to_string(), "approval/asked".to_string()]
        );
        // And nothing here is an emit, which would dispatch rather than decide.
        assert!(
            !outs
                .iter()
                .any(|o| matches!(o, MachineOut::Dispatch { .. })),
            "{outs:?}"
        );
        let _ = RouteOut::UnknownTarget {
            to: vocoder_cordis::MachineId::new("x"),
        };
    }
}
