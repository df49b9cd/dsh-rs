//! Process-local assistant-stream state: the reconnect baseline for a live turn.
//!
//! A client that watches a turn over `session/follow` gets two things: durable
//! events (the session log's rows) and, when it asks for them with
//! `assistantStream: true`, *presentation* frames describing the model's output
//! as it is being produced. The frames are not durable — they describe a partial
//! answer that the settling `assistant/message` row will supersede — so a client
//! reconnecting mid-turn needs a snapshot of them or it would show an empty
//! partial until the next chunk arrived.
//!
//! That snapshot is what upstream calls `SessionAssistantStreamBaseline`:
//! a `revision`, and an `activeAttempt` carrying the attempt's identity, its
//! position, the index its next chunk must have, and its accumulated stream. This
//! module is the state that produces it.
//!
//! ## Why the baseline is compacted rather than a frame list
//!
//! The `stream` in the baseline is the same *compact* record form the durable
//! rows use — `text-chunks` runs with parallel arrays, not a list of frames
//! whose `index` counts up. The reason is the same one the durable form has:
//! a long answer is thousands of deltas, and a baseline the size of the answer's
//! delta count is a snapshot that costs as much to send as re-streaming it.
//! Compaction turns it into the answer's *text*, which is what a client needs.
//!
//! ## The two counters, and why both exist
//!
//! `revision` increments on every frame and is what a client uses to notice that
//! frames were missed or that the attempt restarted. `nextIndex` is the dense
//! position the next `chunk` frame must carry. They look redundant and are not:
//! a `start` frame carries a revision but no index, so a client that joined on a
//! `start` has nothing to derive the chunk position from except the baseline.
//! Upstream's own accumulator rejects a chunk whose `index` is not exactly its
//! expected next, which is why a gap silently blanks the live display rather
//! than showing the answer with a hole in it.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::llm_replay::StreamChunk;

/// One attempt's accumulated state, as a follower would need it to reconnect.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub attempt_id: String,
    /// The durable cursor the attempt started after, so a client can tell
    /// whether the events it holds already cover the attempt's beginning.
    pub started_after_seq: i64,
    pub turn: u64,
    pub step: u64,
    /// The dense index the next `chunk` frame will carry.
    pub next_index: u64,
    /// The frames received so far, in arrival order, for baseline compaction.
    ///
    /// Held as chunks rather than pre-rendered frames because compaction has to
    /// fold *deltas* into runs, and a frame list has already lost the delta
    /// identity: two `text-delta` frames with the same index are one run, and
    /// reconstructing that from frames would mean re-deriving a rule the
    /// producer already knew.
    pub chunks: Vec<(String, Value)>,
}

/// The assistant-stream state of one session.
#[derive(Debug, Default, Clone)]
pub struct AssistantStream {
    /// Increments on every accepted frame; the client's missed-frame detector.
    pub revision: u64,
    /// The attempt currently streaming, if any.
    pub active: Option<Attempt>,
}

impl AssistantStream {
    /// Fold one frame, returning whether it changed anything.
    ///
    /// A frame for a *different* attempt than the active one resets the state:
    /// the protocol's continuity rule is that chunks belong to the attempt named
    /// by the most recent `start`, so a chunk from another attempt cannot be
    /// appended to this one's text without splicing two answers together.
    pub fn accept(&mut self, frame: &Value) -> bool {
        let kind = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let attempt_id = frame
            .get("attemptId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match kind {
            "start" => {
                self.revision = frame
                    .get("revision")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.revision + 1);
                self.active = Some(Attempt {
                    attempt_id,
                    started_after_seq: frame
                        .get("startedAfterSeq")
                        .and_then(Value::as_i64)
                        .unwrap_or(-1),
                    turn: frame.get("turn").and_then(Value::as_u64).unwrap_or(0),
                    step: frame.get("step").and_then(Value::as_u64).unwrap_or(0),
                    next_index: 0,
                    chunks: Vec::new(),
                });
                true
            }
            "chunk" => {
                let index = frame.get("index").and_then(Value::as_u64);
                let Some(active) = self.active.as_mut() else {
                    // A chunk with no attempt open is not appliable: there is
                    // nothing to attribute its text to. Dropping it is what
                    // upstream does, and the alternative — starting an attempt
                    // from a chunk — would invent a `start` the producer never
                    // sent.
                    return false;
                };
                if active.attempt_id != attempt_id {
                    return false;
                }
                // Dense or dropped: a gap means frames were lost, and appending
                // across one would concatenate text the client never saw the
                // middle of.
                if index != Some(active.next_index) {
                    return false;
                }
                self.revision = frame
                    .get("revision")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.revision + 1);
                active.next_index += 1;
                if let Some(chunk) = frame.get("chunk") {
                    active.chunks.push((chunk_type(chunk), chunk.clone()));
                }
                true
            }
            "end" => {
                self.revision = frame
                    .get("revision")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.revision + 1);
                self.active = None;
                true
            }
            _ => false,
        }
    }

    /// The baseline a reconnecting follower receives.
    ///
    /// `revision: 0` with no attempt is the honest answer for a session with no
    /// live turn: it says "nothing has happened", which is exactly what a client
    /// should conclude, and it is what upstream's `EMPTY_BASELINE` is.
    pub fn baseline(&self) -> Value {
        let mut out = json!({ "revision": self.revision });
        if let Some(active) = &self.active {
            out["activeAttempt"] = json!({
                "attemptId": active.attempt_id,
                "startedAfterSeq": active.started_after_seq,
                "turn": active.turn,
                "step": active.step,
                "nextIndex": active.next_index,
                "stream": compact(&active.chunks),
            });
        }
        out
    }
}

/// The chunk's own `type`, for compaction.
fn chunk_type(chunk: &Value) -> String {
    chunk
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Compact frames into the durable `stream` record form.
///
/// Consecutive same-index text and reasoning deltas collapse into
/// `text-chunks`/`reasoning-chunks` runs; tool deltas into `tool-call-chunks`;
/// everything else stays a raw `chunk`. This is the *same* encoding
/// `agent::chunk_records` produces for a settled turn's durable row, and it is
/// deliberate that both exist rather than one calling the other: the durable
/// path has `StreamChunk` values to fold, while this has already-serialized
/// frames, and converting between them to share a function would mean
/// re-parsing JSON to re-encode it.
///
/// The parallel-array shape (`texts` plus `dt` gaps) is not reproduced here:
/// upstream packs per-member timing gaps, and this host has no per-delta
/// timestamps for a live frame beyond what `session/follow` already carries on
/// each frame. Emitting the array without the gaps is the honest reading —
/// a fabricated gap would claim a timing the producer never measured.
fn compact(chunks: &[(String, Value)]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < chunks.len() {
        let (kind, chunk) = &chunks[i];
        match kind.as_str() {
            "text-delta" | "reasoning-delta" => {
                let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
                let mut texts = Vec::new();
                while i < chunks.len() {
                    let (k, c) = &chunks[i];
                    if k != kind || c.get("index").and_then(Value::as_u64) != Some(index) {
                        break;
                    }
                    if let Some(text) = c.get("text").and_then(Value::as_str) {
                        texts.push(json!(text));
                    }
                    i += 1;
                }
                out.push(json!({
                    "type": if kind == "text-delta" { "text-chunks" } else { "reasoning-chunks" },
                    "index": index,
                    "texts": texts,
                }));
            }
            "tool-call-delta" => {
                let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
                let id = chunk
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = chunk
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let mut args = Vec::new();
                while i < chunks.len() {
                    let (k, c) = &chunks[i];
                    if k != kind || c.get("index").and_then(Value::as_u64) != Some(index) {
                        break;
                    }
                    args.push(json!(
                        c.get("argumentsDelta")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    ));
                    i += 1;
                }
                let mut record = json!({
                    "type": "tool-call-chunks", "index": index, "id": id, "args": args,
                });
                if let Some(name) = name {
                    record["name"] = json!(name);
                }
                out.push(record);
            }
            _ => {
                out.push(json!({ "type": "chunk", "chunk": chunk }));
                i += 1;
            }
        }
    }
    out
}

/// The accumulated display text of a chunk list, for a test or a caller that
/// wants the answer rather than the encoding.
#[cfg(test)]
pub fn joined_text(chunks: &[(String, Value)]) -> String {
    let mut out = String::new();
    for (kind, chunk) in chunks {
        if kind == "text-delta"
            && let Some(text) = chunk.get("text").and_then(Value::as_str)
        {
            out.push_str(text);
        }
    }
    out
}

/// The accumulated text of every block in a [`StreamChunk`] list, in index
/// order — the same fold `agent::assemble_message` performs.
///
/// Unused by the machine itself; it exists so a test can assert that the
/// streaming path and the buffered path agree about what the model said, which
/// is the property that makes two decoders acceptable where one would be safer.
#[allow(dead_code)]
pub fn fold_blocks(chunks: &[StreamChunk]) -> BTreeMap<u64, String> {
    let mut blocks: BTreeMap<u64, String> = BTreeMap::new();
    for chunk in chunks {
        let (index, text) = match chunk {
            StreamChunk::TextDelta { index, text }
            | StreamChunk::ReasoningDelta { index, text } => (*index, text.as_str()),
            _ => continue,
        };
        blocks.entry(index).or_default().push_str(text);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(attempt: &str, revision: u64) -> Value {
        json!({
            "type": "start", "attemptId": attempt, "revision": revision,
            "startedAfterSeq": 3, "turn": 1, "step": 1,
        })
    }

    fn chunk(attempt: &str, revision: u64, index: u64, chunk: Value) -> Value {
        json!({
            "type": "chunk", "attemptId": attempt, "revision": revision,
            "index": index, "time": 0, "chunk": chunk,
        })
    }

    /// A chunk that arrives before any `start` is dropped, not applied.
    ///
    /// There is no attempt to attribute it to, and inventing one would hand a
    /// client a partial whose attempt id no `start` frame ever named — the
    /// client's own continuity check would then reject everything after it.
    #[test]
    fn a_chunk_before_any_start_is_dropped() {
        let mut s = AssistantStream::default();
        assert!(!s.accept(&chunk(
            "a",
            1,
            0,
            json!({ "type": "text-delta", "index": 0, "text": "x" })
        )));
        assert!(s.active.is_none());
        assert_eq!(s.revision, 0);
    }

    /// A chunk whose index is not the expected next is dropped, and the attempt
    /// stays where it was.
    ///
    /// A gap means frames were lost; appending across one would concatenate text
    /// whose middle the client never saw, which renders as a plausible answer
    /// that the model never produced.
    #[test]
    fn a_gap_in_the_chunk_indices_is_dropped() {
        let mut s = AssistantStream::default();
        assert!(s.accept(&start("a", 1)));
        assert!(s.accept(&chunk(
            "a",
            2,
            0,
            json!({ "type": "text-delta", "index": 0, "text": "one" })
        )));
        // Index 2, skipping 1.
        assert!(!s.accept(&chunk(
            "a",
            3,
            2,
            json!({ "type": "text-delta", "index": 0, "text": "three" })
        )));
        let active = s.active.as_ref().expect("still active");
        assert_eq!(active.next_index, 1, "the expected next index is unchanged");
        assert_eq!(joined_text(&active.chunks), "one");
    }

    /// A chunk naming a different attempt than the active one is dropped.
    ///
    /// This is the splice case: two attempts' deltas are both `text-delta` with
    /// `index: 0`, so appending the second to the first produces one coherent
    /// paragraph drawn from two different answers.
    #[test]
    fn a_chunk_from_another_attempt_is_dropped() {
        let mut s = AssistantStream::default();
        assert!(s.accept(&start("a", 1)));
        assert!(s.accept(&chunk(
            "a",
            2,
            0,
            json!({ "type": "text-delta", "index": 0, "text": "from a" })
        )));
        assert!(!s.accept(&chunk(
            "b",
            3,
            1,
            json!({ "type": "text-delta", "index": 0, "text": "from b" })
        )));
        assert_eq!(joined_text(&s.active.as_ref().unwrap().chunks), "from a");
    }

    /// A `start` resets the state even mid-attempt, and the baseline reflects the
    /// new attempt rather than a merge of the two.
    #[test]
    fn a_start_replaces_the_active_attempt() {
        let mut s = AssistantStream::default();
        assert!(s.accept(&start("a", 1)));
        assert!(s.accept(&chunk(
            "a",
            2,
            0,
            json!({ "type": "text-delta", "index": 0, "text": "old" })
        )));
        assert!(s.accept(&start("b", 3)));
        let active = s.active.as_ref().unwrap();
        assert_eq!(active.attempt_id, "b");
        assert_eq!(active.next_index, 0);
        assert!(active.chunks.is_empty());
    }

    /// An `end` clears the active attempt, so a later chunk is dropped rather
    /// than appended to a finished answer.
    #[test]
    fn end_clears_the_attempt() {
        let mut s = AssistantStream::default();
        assert!(s.accept(&start("a", 1)));
        assert!(s.accept(&json!({ "type": "end", "attemptId": "a", "revision": 2, "index": 1, "outcome": { "kind": "committed", "eventType": "assistant/message", "seq": 4 } })));
        assert!(s.active.is_none());
        assert!(!s.accept(&chunk(
            "a",
            3,
            1,
            json!({ "type": "text-delta", "index": 0, "text": "late" })
        )));
    }

    /// The baseline carries the attempt's identity, position, and *compacted*
    /// text — which is what a reconnecting follower needs to render the partial
    /// without re-receiving every delta.
    #[test]
    fn the_baseline_carries_a_compacted_stream() {
        let mut s = AssistantStream::default();
        assert!(s.accept(&start("a", 1)));
        for (i, text) in ["Here ", "is **bo", "ld**"].iter().enumerate() {
            assert!(s.accept(&chunk(
                "a",
                i as u64 + 2,
                i as u64,
                json!({ "type": "text-delta", "index": 0, "text": text })
            )));
        }
        let b = s.baseline();
        assert_eq!(b["activeAttempt"]["attemptId"], "a");
        assert_eq!(b["activeAttempt"]["nextIndex"], 3);
        assert_eq!(b["activeAttempt"]["startedAfterSeq"], 3);
        // One `text-chunks` run, not three raw chunks: the compact form is the
        // whole point of a baseline — a delta-per-entry snapshot costs as much
        // as the stream it replaces.
        let stream = b["activeAttempt"]["stream"].as_array().unwrap();
        assert_eq!(stream.len(), 1, "deltas compact into one run: {stream:?}");
        assert_eq!(stream[0]["type"], "text-chunks");
        assert_eq!(
            stream[0]["texts"],
            json!(["Here ", "is **bo", "ld**"]),
            "the run keeps every fragment in order"
        );
    }

    /// A session with no live turn reports revision 0 and no attempt.
    ///
    /// The absence of `activeAttempt` is the signal: an `activeAttempt` with
    /// empty fields would claim a turn is running.
    #[test]
    fn an_idle_session_has_an_empty_baseline() {
        let s = AssistantStream::default();
        let b = s.baseline();
        assert_eq!(b["revision"], 0);
        assert!(b.get("activeAttempt").is_none());
    }

    /// Tool-call fragments compact into a run carrying the id and name, so a
    /// reconnecting client can assemble the arguments it missed.
    #[test]
    fn tool_fragments_compact_in_order() {
        let mut s = AssistantStream::default();
        assert!(s.accept(&start("a", 1)));
        assert!(s.accept(&chunk(
            "a",
            2,
            0,
            json!({
                "type": "tool-call-delta", "index": 0, "id": "call_1", "name": "write",
                "argumentsDelta": "{\"a\":"
            })
        )));
        assert!(s.accept(&chunk(
            "a",
            3,
            1,
            json!({
                "type": "tool-call-delta", "index": 0, "id": "call_1", "argumentsDelta": "1}"
            })
        )));
        let stream = s.baseline()["activeAttempt"]["stream"].clone();
        let run = &stream.as_array().unwrap()[0];
        assert_eq!(run["type"], "tool-call-chunks");
        assert_eq!(run["id"], "call_1");
        assert_eq!(run["name"], "write");
        assert_eq!(run["args"], json!(["{\"a\":", "1}"]));
    }

    /// The revision advances on every accepted frame, so a client can tell that
    /// it missed one.
    #[test]
    fn the_revision_advances_with_each_frame() {
        let mut s = AssistantStream::default();
        assert!(s.accept(&start("a", 1)));
        assert_eq!(s.revision, 1);
        assert!(s.accept(&chunk(
            "a",
            2,
            0,
            json!({ "type": "text-delta", "index": 0, "text": "x" })
        )));
        assert_eq!(s.revision, 2);
        // A dropped frame must not advance it: the client would otherwise
        // believe it had seen something it did not.
        assert!(!s.accept(&chunk(
            "a",
            3,
            9,
            json!({ "type": "text-delta", "index": 0, "text": "y" })
        )));
        assert_eq!(s.revision, 2);
    }
}
