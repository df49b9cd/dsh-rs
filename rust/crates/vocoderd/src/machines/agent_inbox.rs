//! The agent inbox: pending input, as a fold over durable splices.
//!
//! Upstream (`dsh/packages/core/agent-loop/src/inbox.ts`) keeps pending input in
//! two lists — `next-turn` (prompts awaiting a turn) and `next-step` (input
//! awaiting the next step boundary) — and stores it *in the session log* as
//! `agent/inbox/spliced` rows. The in-memory state is a projection folded from
//! those rows, so a restart reconstructs it rather than losing it.
//!
//! This is the loop's input side. The FSM in [`super::agent_loop`] decides what
//! a turn *does*; this decides what it has to work on. Together they are the
//! whole of upstream's driver minus the model call itself.
//!
//! ## Why the fold validates rather than trusts
//!
//! The rows cross a durable boundary, so a splice is read back as untyped JSON.
//! Upstream's folder *rejects* a malformed one — a `start` outside the list, a
//! negative count, a range that runs past the end — because a silently-clamped
//! splice would make pending input appear to move when it did not, and the
//! caller has no way to notice. It also rejects a **duplicate identity** across
//! both lists: a message pending twice would be delivered twice.
//!
//! [`Inbox::fold`] applies the same rules and reports the same class of failure,
//! so a corrupt log is a typed error rather than a wrong answer.
//!
//! ## What is not here
//!
//! The FSM's `has_tool_calls` input and `more_input` argument are how this
//! reaches it; wiring the two together — and attaching both to a namespace
//! machine — is what remains of step 2. `claim` is implemented because the FSM
//! cannot be driven without it, but nothing calls it yet outside tests.

// Same situation as `agent_loop` and `llm_replay`: complete and tested, not yet
// wired to a namespace machine. One scoped allow rather than item-level ones.
#![allow(dead_code)]

use serde_json::{Value, json};

/// Which pending list a splice addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxTarget {
    /// Prompts awaiting individual turns.
    NextTurn,
    /// Input awaiting the next step boundary.
    NextStep,
}

impl InboxTarget {
    /// The wire spelling of this target.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NextTurn => "next-turn",
            Self::NextStep => "next-step",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "next-turn" => Some(Self::NextTurn),
            "next-step" => Some(Self::NextStep),
            _ => None,
        }
    }
}

/// Why a splice row could not be folded.
///
/// Each variant is a state the caller can act on — the log is damaged — rather
/// than a message to log and carry on past.
#[derive(Debug, Clone, PartialEq)]
pub enum FoldError {
    /// The target was neither `next-turn` nor `next-step`.
    UnknownTarget { target: String },
    /// `start` was not a position in the list, or ran past its end.
    BadRange {
        start: i64,
        removed: i64,
        len: usize,
    },
    /// A message would be pending in both lists at once, or twice in one.
    DuplicateId { id: String },
}

impl std::fmt::Display for FoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTarget { target } => write!(f, "unknown inbox target {target:?}"),
            Self::BadRange {
                start,
                removed,
                len,
            } => write!(
                f,
                "splice reaches outside the list: start {start}, removed {removed}, length {len}"
            ),
            Self::DuplicateId { id } => write!(f, "message {id:?} is already pending"),
        }
    }
}

impl std::error::Error for FoldError {}

/// Pending input, in the two lists upstream keeps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inbox {
    next_turn: Vec<Value>,
    next_step: Vec<Value>,
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconstruct pending input by folding a session log's rows.
    ///
    /// Only `agent/inbox/spliced` rows matter; every other row is skipped, so a
    /// whole log can be handed in. A malformed splice fails the fold rather
    /// than being clamped — see the module doc for why.
    pub fn fold(rows: &[Value]) -> Result<Self, FoldError> {
        let mut inbox = Self::new();
        for row in rows {
            if row.get("type").and_then(Value::as_str) != Some("agent/inbox/spliced") {
                continue;
            }
            let data = row.get("data").cloned().unwrap_or(Value::Null);
            inbox.apply_splice(&data)?;
        }
        Ok(inbox)
    }

    /// Apply one splice row's `data`.
    fn apply_splice(&mut self, data: &Value) -> Result<(), FoldError> {
        let target = data
            .get("target")
            .and_then(Value::as_str)
            .and_then(InboxTarget::parse)
            .ok_or_else(|| FoldError::UnknownTarget {
                target: data
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })?;
        let list = match target {
            InboxTarget::NextTurn => &mut self.next_turn,
            InboxTarget::NextStep => &mut self.next_step,
        };
        let start = data.get("start").and_then(Value::as_i64).unwrap_or(-1);
        // `removedCount` is optional and defaults to 0, matching upstream's
        // `splice.removedCount ?? 0`.
        let removed = data
            .get("removedCount")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let inserted: Vec<Value> = data
            .get("inserted")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // The range must be a real position in the list. `splice` semantics
        // allow `start == len` for a pure append, which is why the bound is
        // inclusive of the length.
        if start < 0
            || removed < 0
            || start as usize > list.len()
            || start as usize + removed as usize > list.len()
        {
            return Err(FoldError::BadRange {
                start,
                removed,
                len: list.len(),
            });
        }
        let start = start as usize;
        let removed = removed as usize;
        list.splice(start..start + removed, inserted);

        self.check_identities()
    }

    /// No message may be pending twice, across either list.
    ///
    /// Checked after every splice, not just at the end of a fold, so a log that
    /// duplicates an identity mid-way is caught at the row that did it.
    fn check_identities(&self) -> Result<(), FoldError> {
        let mut seen = std::collections::BTreeSet::new();
        for message in self.next_turn.iter().chain(self.next_step.iter()) {
            let id = message
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                // An unidentified message has no identity to collide on, so it
                // is not a duplicate. Upstream keys the check on `message.id`,
                // which a caller-minted message always has.
                continue;
            }
            if !seen.insert(id.clone()) {
                return Err(FoldError::DuplicateId { id });
            }
        }
        Ok(())
    }

    /// Prompts awaiting individual turns.
    pub fn next_turn(&self) -> &[Value] {
        &self.next_turn
    }

    /// Input awaiting the next step boundary.
    pub fn next_step(&self) -> &[Value] {
        &self.next_step
    }

    /// Whether either list holds work.
    pub fn has_pending(&self) -> bool {
        !self.next_turn.is_empty() || !self.next_step.is_empty()
    }

    /// Take the batch one step should run on, and drop it from the lists.
    ///
    /// Mirrors upstream's `claim`: every `next-step` message is claimed, and
    /// when `target` is a turn boundary *one* queued `next-turn` message is
    /// claimed alongside it. The batch is `next-step` first, then the queued
    /// turn — an order the FSM's message list depends on, since the queued
    /// prompt is what the step is answering.
    ///
    /// Returns the splice rows that would record the claim, so the caller
    /// publishes them to the log rather than the inbox mutating hidden state.
    pub fn claim(&mut self, target: InboxTarget, _turn: u64) -> (Vec<Value>, Vec<Splice>) {
        let mut claimed = std::mem::take(&mut self.next_step);
        let mut splices = vec![Splice {
            target: InboxTarget::NextStep,
            start: 0,
            removed: claimed.len(),
            inserted: vec![],
        }];
        if target == InboxTarget::NextTurn && !self.next_turn.is_empty() {
            let queued = self.next_turn.remove(0);
            claimed.push(queued);
            splices.push(Splice {
                target: InboxTarget::NextTurn,
                start: 0,
                removed: 1,
                inserted: vec![],
            });
        }
        (claimed, splices)
    }

    /// Append one message to a list, returning the splice to record.
    pub fn append(&mut self, target: InboxTarget, message: Value) -> Splice {
        let list = match target {
            InboxTarget::NextTurn => &mut self.next_turn,
            InboxTarget::NextStep => &mut self.next_step,
        };
        let start = list.len();
        list.push(message);
        Splice {
            target,
            start,
            removed: 0,
            inserted: vec![list[start].clone()],
        }
    }

    /// Remove one pending message by id, returning the splice or `None`.
    pub fn remove(&mut self, message_id: &str) -> Option<Splice> {
        for target in [InboxTarget::NextStep, InboxTarget::NextTurn] {
            let list = match target {
                InboxTarget::NextTurn => &mut self.next_turn,
                InboxTarget::NextStep => &mut self.next_step,
            };
            if let Some(index) = list
                .iter()
                .position(|m| m.get("id").and_then(Value::as_str) == Some(message_id))
            {
                list.remove(index);
                return Some(Splice {
                    target,
                    start: index,
                    removed: 1,
                    inserted: vec![],
                });
            }
        }
        None
    }
}

/// One `agent/inbox/spliced` row to record.
///
/// The inbox hands these back rather than appending them itself: a machine may
/// not write the log (that is an effect), and keeping the row's construction
/// here rather than in the caller is what makes [`Inbox::fold`] and the writers
/// agree on a shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Splice {
    pub target: InboxTarget,
    pub start: usize,
    /// How many messages were removed.
    pub removed: usize,
    /// What was inserted at `start`.
    pub inserted: Vec<Value>,
}

impl Splice {
    /// The row `data` as the log records it.
    ///
    /// `removedCount` is omitted when zero, matching how upstream writes it.
    pub fn to_data(&self) -> Value {
        let mut v = json!({
            "target": self.target.as_str(),
            "start": self.start,
            "inserted": self.inserted,
        });
        if self.removed > 0 {
            v["removedCount"] = self.removed.into();
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "role": "user",
            "content": [{ "type": "text", "text": text }],
            "source": { "kind": "user" },
        })
    }

    fn splice_row(target: &str, start: i64, removed: i64, inserted: Vec<Value>) -> Value {
        json!({
            "type": "agent/inbox/spliced",
            "data": {
                "target": target,
                "start": start,
                "removedCount": removed,
                "inserted": inserted,
            },
        })
    }

    // -- folding -------------------------------------------------------------

    /// An append lands in the list it names, at the position it names.
    #[test]
    fn an_append_lands_in_its_list() {
        let rows = vec![
            splice_row("next-turn", 0, 0, vec![message("m1", "first")]),
            splice_row("next-step", 0, 0, vec![message("m2", "steer")]),
        ];
        let inbox = Inbox::fold(&rows).expect("foldable");
        assert_eq!(inbox.next_turn().len(), 1);
        assert_eq!(inbox.next_step().len(), 1);
        assert_eq!(inbox.next_step()[0]["id"], "m2");
        assert!(inbox.has_pending());
    }

    /// The two lists are independent: a claim on one does not touch the other.
    #[test]
    fn the_two_lists_are_independent() {
        let rows = vec![
            splice_row(
                "next-turn",
                0,
                0,
                vec![message("a", "one"), message("b", "two")],
            ),
            splice_row("next-step", 0, 0, vec![message("c", "three")]),
        ];
        let inbox = Inbox::fold(&rows).expect("foldable");
        assert_eq!(inbox.next_turn().len(), 2);
        assert_eq!(inbox.next_step().len(), 1);
    }

    /// A splice replaces the range it names, with `splice` semantics.
    #[test]
    fn a_splice_replaces_the_named_range() {
        let rows = vec![
            splice_row(
                "next-turn",
                0,
                0,
                vec![
                    message("a", "one"),
                    message("b", "two"),
                    message("c", "three"),
                ],
            ),
            // Replace index 1 with a different message.
            splice_row("next-turn", 1, 1, vec![message("d", "replaced")]),
        ];
        let inbox = Inbox::fold(&rows).expect("foldable");
        let ids: Vec<&str> = inbox
            .next_turn()
            .iter()
            .map(|m| m["id"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["a", "d", "c"]);
    }

    /// A removal leaves the other messages in order.
    #[test]
    fn a_removal_leaves_the_rest_in_order() {
        let rows = vec![
            splice_row(
                "next-step",
                0,
                0,
                vec![message("a", "1"), message("b", "2"), message("c", "3")],
            ),
            splice_row("next-step", 1, 1, vec![]),
        ];
        let inbox = Inbox::fold(&rows).expect("foldable");
        let ids: Vec<&str> = inbox
            .next_step()
            .iter()
            .map(|m| m["id"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["a", "c"]);
    }

    /// A splice at `start == len` is a pure append, which the range check must
    /// allow — it is the most common shape.
    #[test]
    fn an_append_at_the_end_is_allowed() {
        let rows = vec![
            splice_row("next-turn", 0, 0, vec![message("a", "1")]),
            splice_row("next-turn", 1, 0, vec![message("b", "2")]),
        ];
        assert_eq!(Inbox::fold(&rows).expect("foldable").next_turn().len(), 2);
    }

    /// `removedCount` is optional and defaults to zero.
    #[test]
    fn an_absent_removed_count_means_zero() {
        let rows = vec![json!({
            "type": "agent/inbox/spliced",
            "data": { "target": "next-turn", "start": 0, "inserted": [message("a", "1")] },
        })];
        assert_eq!(Inbox::fold(&rows).expect("foldable").next_turn().len(), 1);
    }

    // -- the fold rejects damage rather than clamping ------------------------

    /// A `start` past the end is refused, not clamped: a clamped splice would
    /// move pending input somewhere the caller did not ask for.
    #[test]
    fn a_start_past_the_end_is_refused() {
        let rows = vec![splice_row("next-turn", 5, 0, vec![message("a", "1")])];
        assert!(matches!(
            Inbox::fold(&rows),
            Err(FoldError::BadRange { start: 5, .. })
        ));
    }

    /// A range that runs past the end is refused.
    #[test]
    fn a_range_running_past_the_end_is_refused() {
        let rows = vec![
            splice_row("next-turn", 0, 0, vec![message("a", "1")]),
            splice_row("next-turn", 0, 3, vec![]),
        ];
        assert!(matches!(
            Inbox::fold(&rows),
            Err(FoldError::BadRange {
                removed: 3,
                len: 1,
                ..
            })
        ));
    }

    /// A negative position or count is refused.
    #[test]
    fn a_negative_position_or_count_is_refused() {
        for (start, removed) in [(-1, 0), (0, -1)] {
            let rows = vec![splice_row("next-turn", start, removed, vec![])];
            assert!(
                matches!(Inbox::fold(&rows), Err(FoldError::BadRange { .. })),
                "start {start} removed {removed} should be refused"
            );
        }
    }

    /// An unknown target is refused rather than silently ignored: a splice that
    /// went nowhere is exactly the kind of damage that reads as success.
    #[test]
    fn an_unknown_target_is_refused() {
        let rows = vec![splice_row("next-century", 0, 0, vec![message("a", "1")])];
        assert!(matches!(
            Inbox::fold(&rows),
            Err(FoldError::UnknownTarget { .. })
        ));
    }

    /// A message pending twice is refused — it would be delivered twice.
    #[test]
    fn a_duplicate_identity_is_refused() {
        let rows = vec![
            splice_row("next-turn", 0, 0, vec![message("same", "1")]),
            splice_row("next-step", 0, 0, vec![message("same", "2")]),
        ];
        assert!(matches!(
            Inbox::fold(&rows),
            Err(FoldError::DuplicateId { .. })
        ));
    }

    /// The duplicate check runs *after every splice*, so the failure is
    /// attributed to the row that introduced it rather than to the whole log.
    #[test]
    fn a_duplicate_is_caught_at_the_row_that_introduced_it() {
        let mut rows = vec![splice_row("next-turn", 0, 0, vec![message("a", "1")])];
        rows.push(splice_row("next-turn", 0, 0, vec![message("a", "again")]));
        // The second splice inserts a second copy of "a" at position 0.
        let err = Inbox::fold(&rows).expect_err("duplicate must be refused");
        assert_eq!(err, FoldError::DuplicateId { id: "a".into() });
    }

    // -- claim ---------------------------------------------------------------

    /// A turn-boundary claim takes every `next-step` message plus exactly one
    /// queued turn, with `next-step` first.
    #[test]
    fn a_turn_claim_takes_the_step_batch_and_one_queued_turn() {
        let rows = vec![
            splice_row("next-step", 0, 0, vec![message("s1", "steer")]),
            splice_row(
                "next-turn",
                0,
                0,
                vec![message("t1", "prompt"), message("t2", "later")],
            ),
        ];
        let mut inbox = Inbox::fold(&rows).expect("foldable");
        let (claimed, splices) = inbox.claim(InboxTarget::NextTurn, 1);
        let ids: Vec<&str> = claimed
            .iter()
            .map(|m| m["id"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["s1", "t1"], "next-step first, then one turn");
        assert_eq!(inbox.next_turn().len(), 1, "t2 stays queued");
        assert_eq!(inbox.next_step().len(), 0);
        assert_eq!(splices.len(), 2);
    }

    /// A step-boundary claim takes only the `next-step` batch and leaves the
    /// queued turn alone — that is the difference between the two targets.
    #[test]
    fn a_step_claim_leaves_the_queued_turn_alone() {
        let rows = vec![
            splice_row("next-step", 0, 0, vec![message("s1", "steer")]),
            splice_row("next-turn", 0, 0, vec![message("t1", "prompt")]),
        ];
        let mut inbox = Inbox::fold(&rows).expect("foldable");
        let (claimed, splices) = inbox.claim(InboxTarget::NextStep, 1);
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0]["id"], "s1");
        assert_eq!(inbox.next_turn().len(), 1, "the queued turn is untouched");
        assert_eq!(splices.len(), 1, "only the step splice is owed");
    }

    /// A claim with nothing pending yields nothing and owes no rows.
    #[test]
    fn an_empty_claim_yields_nothing() {
        let mut inbox = Inbox::new();
        let (claimed, splices) = inbox.claim(InboxTarget::NextTurn, 1);
        assert!(claimed.is_empty());
        assert_eq!(
            splices.len(),
            1,
            "the (empty) next-step splice is still owed"
        );
        assert_eq!(splices[0].removed, 0);
        assert!(!inbox.has_pending());
    }

    // -- append / remove -----------------------------------------------------

    /// An append's splice records exactly what was inserted, so folding the
    /// splice reproduces the state the append produced.
    #[test]
    fn an_append_splice_reproduces_its_state_when_folded() {
        let mut inbox = Inbox::new();
        let splice = inbox.append(InboxTarget::NextTurn, message("a", "1"));
        assert_eq!(splice.start, 0);
        assert_eq!(splice.removed, 0);

        let rows = vec![json!({
            "type": "agent/inbox/spliced",
            "data": splice.to_data(),
        })];
        let folded = Inbox::fold(&rows).expect("foldable");
        assert_eq!(folded, inbox, "the splice round-trips through the fold");
    }

    /// A removal finds its message in either list, and reports which.
    #[test]
    fn a_removal_finds_its_message_in_either_list() {
        let mut inbox = Inbox::new();
        inbox.append(InboxTarget::NextTurn, message("t", "1"));
        inbox.append(InboxTarget::NextStep, message("s", "2"));

        let removed = inbox.remove("s").expect("present in next-step");
        assert_eq!(removed.target, InboxTarget::NextStep);
        assert_eq!(removed.start, 0);

        let removed = inbox.remove("t").expect("present in next-turn");
        assert_eq!(removed.target, InboxTarget::NextTurn);
        assert!(!inbox.has_pending());
    }

    /// Removing something that is not pending reports that, rather than
    /// removing a neighbour.
    #[test]
    fn a_removal_of_something_absent_is_reported() {
        let mut inbox = Inbox::new();
        inbox.append(InboxTarget::NextTurn, message("a", "1"));
        assert!(inbox.remove("nope").is_none());
        assert_eq!(inbox.next_turn().len(), 1, "the list is untouched");
    }

    /// `removedCount` is omitted from the row when nothing was removed — the
    /// shape upstream writes, which the fold must also accept.
    #[test]
    fn a_zero_removal_omits_the_count() {
        let splice = Splice {
            target: InboxTarget::NextTurn,
            start: 0,
            removed: 0,
            inserted: vec![message("a", "1")],
        };
        let data = splice.to_data();
        assert!(data.get("removedCount").is_none(), "{data}");
    }

    // -- one real log row ----------------------------------------------------

    /// The fold accepts the row shape the corpus actually records, which packs
    /// the inserted message whole.
    #[test]
    fn it_folds_a_real_corpus_row() {
        let row = json!({
            "type": "agent/inbox/spliced",
            "data": {
                "target": "next-turn",
                "start": 0,
                "inserted": [{
                    "content": [{ "type": "text", "text": "Reply with exactly DIRECT_CHILD_OK and nothing else." }],
                    "source": { "kind": "user" },
                    "role": "user",
                    "id": "{{message:12}}"
                }]
            }
        });
        let inbox = Inbox::fold(&[row]).expect("foldable");
        assert_eq!(inbox.next_turn().len(), 1);
        assert_eq!(inbox.next_turn()[0]["id"], "{{message:12}}");
    }

    // -- the corpus ----------------------------------------------------------

    /// Fold every committed snapshot's inbox rows.
    ///
    /// The point is not that the fold returns *something* — it is that every
    /// recorded splice is well-formed by the rules the fold enforces. A corpus
    /// row the fold refuses would mean either the rules are wrong or the
    /// recorder writes rows a reader cannot replay, and both matter more than
    /// any single assertion here.
    ///
    /// It also counts the rows actually seen, so a corpus that stopped
    /// exercising splices is visible rather than silently passing.
    #[test]
    fn every_committed_snapshot_has_a_foldable_inbox() {
        use std::path::Path;

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dsh/snapshots/session");
        if !root.is_dir() {
            eprintln!(
                "skipping: {} absent (submodule not checked out)",
                root.display()
            );
            return;
        }

        let mut dirs = 0usize;
        let mut splice_rows = 0usize;
        let mut pending_total = 0usize;
        for entry in std::fs::read_dir(&root).expect("read snapshots") {
            let dir = entry.expect("dirent").path();
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
            dirs += 1;
            splice_rows += rows
                .iter()
                .filter(|r| r.get("type").and_then(Value::as_str) == Some("agent/inbox/spliced"))
                .count();
            let inbox = Inbox::fold(&rows)
                .unwrap_or_else(|e| panic!("{}: inbox rows are not foldable: {e}", dir.display()));
            pending_total += inbox.next_turn().len() + inbox.next_step().len();
        }

        eprintln!(
            "folded {dirs} sessions, {splice_rows} splice rows, {pending_total} messages left pending"
        );
        assert!(dirs >= 40, "expected many snapshots, saw {dirs}");
        assert!(
            splice_rows >= 100,
            "the corpus should exercise splices heavily, saw {splice_rows}"
        );
    }
}
