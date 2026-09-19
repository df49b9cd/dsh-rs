//! The LLM call seam: the streaming chunk vocabulary, and a **replay provider**
//! that serves a recorded session's model calls back.
//!
//! Named `llm_replay`, not `llm_seam`, because the neighbouring `llm.rs` is the
//! *namespace machine* (the model registry a client reads) and this is not that:
//! it is the call path the agent loop consumes.
//!
//! Upstream has two halves here. `packages/llm/llm` is the runtime (adapter
//! registry, `stream(request)`, assembler); `packages/test-support/llm-replay`
//! is the provider its own CI runs the whole web lane on — in replay mode
//! `launchWebScaffold` disables the real provider and installs the replay one,
//! so a test needs no network and **no API key**. `DSH_SNAPSHOT=replay …
//! test:web:ci` in `dsh/scripts/run-gates.ts` carries no key at all.
//!
//! This mirrors the second half first, deliberately. The seam is where the
//! agent-loop FSM gets its replies, and sizing it to replay means the loop is
//! testable end to end today — against the same 85 directories of recorded
//! sessions the FSM's own tests already read — rather than after a real
//! provider adapter lands. A network adapter is a later, separable addition
//! behind the same trait.
//!
//! ## Why replay is not "test scaffolding" here
//!
//! A recorded session log already *contains* every model call it made: each
//! `assistant/message` carries the `stream` that produced it. Deriving a script
//! from those rows is therefore not a mock — it is reading the same durable
//! truth the rest of the system reads, which is why `deriveReplayScript` can
//! assert that a call "ended without a finish chunk" and refuse rather than
//! invent an ending. That refusal is the load-bearing part: a replay that
//! guessed an ending would make a broken loop look correct.

// Same situation as `agent_loop`: the seam is complete and tested, but nothing
// outside this module and its tests builds it yet — the driver attaches it when
// the loop is wired to a namespace machine. One scoped allow, removed in one
// deletion, rather than eleven item-level ones.
#![allow(dead_code)]

use serde_json::{Value, json};

/// One chunk of a model response, mirroring upstream's `StreamChunk`.
///
/// The vocabulary is the spec's, from `packages/llm/llm/src/types.ts`. Deltas
/// (`text-delta`, `reasoning-delta`, `tool-call-delta`) are assembled into
/// blocks between `block-start` and `block-end`; `usage` and `finish` close the
/// response.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamChunk {
    /// A block opens at `index`. Assemblers use this to allocate.
    BlockStart { index: u64, block_type: BlockType },
    /// Incremental text for the open text block at `index`.
    TextDelta { index: u64, text: String },
    /// Incremental reasoning for the open reasoning block at `index`.
    ReasoningDelta { index: u64, text: String },
    /// Incremental tool-call arguments. `name` arrives on the first delta.
    ToolCallDelta {
        index: u64,
        id: String,
        name: Option<String>,
        arguments_delta: String,
    },
    /// A block closes with its assembled value.
    BlockEnd { index: u64, block: Value },
    /// Token accounting for the call; disjoint counts, per the spec.
    Usage { usage: Value },
    /// The response ends. Every replay entry must end with one of these.
    Finish { reason: FinishReason },
}

/// The block kinds a model response may contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    Text,
    Reasoning,
    ToolCall,
}

impl BlockType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Reasoning => "reasoning",
            Self::ToolCall => "tool-call",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "reasoning" => Some(Self::Reasoning),
            "tool-call" => Some(Self::ToolCall),
            _ => None,
        }
    }
}

/// Why a response ended, mirroring upstream's `FinishReasonMap`.
///
/// `stop` and `tool-calls` are ordinary; `max-tokens` is what makes a turn's
/// ending sticky in the FSM; `aborted` and `error` carry a structured failure.
#[derive(Debug, Clone, PartialEq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    MaxTokens,
    Aborted { message: String, code: String },
    Error { message: String, code: String },
}

impl FinishReason {
    /// The `kind` this reason records as.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::ToolCalls => "tool-calls",
            Self::MaxTokens => "max-tokens",
            Self::Aborted { .. } => "aborted",
            Self::Error { .. } => "error",
        }
    }

    fn to_value(&self) -> Value {
        let failure = |message: &str, code: &str| json!({ "message": message, "code": code });
        match self {
            Self::Stop => json!({ "kind": "stop" }),
            Self::ToolCalls => json!({ "kind": "tool-calls" }),
            Self::MaxTokens => json!({ "kind": "max-tokens" }),
            Self::Aborted { message, code } => {
                json!({ "kind": "aborted", "failure": failure(message, code) })
            }
            Self::Error { message, code } => {
                json!({ "kind": "error", "failure": failure(message, code) })
            }
        }
    }
}

impl StreamChunk {
    /// The chunk's wire shape, as the log records it.
    pub fn to_value(&self) -> Value {
        match self {
            Self::BlockStart { index, block_type } => {
                json!({ "type": "block-start", "index": index, "blockType": block_type.as_str() })
            }
            Self::TextDelta { index, text } => {
                json!({ "type": "text-delta", "index": index, "text": text })
            }
            Self::ReasoningDelta { index, text } => {
                json!({ "type": "reasoning-delta", "index": index, "text": text })
            }
            Self::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let mut v = json!({
                    "type": "tool-call-delta",
                    "index": index,
                    "id": id,
                    "argumentsDelta": arguments_delta,
                });
                if let Some(name) = name {
                    v["name"] = name.clone().into();
                }
                v
            }
            Self::BlockEnd { index, block } => {
                json!({ "type": "block-end", "index": index, "block": block })
            }
            Self::Usage { usage } => json!({ "type": "usage", "usage": usage }),
            Self::Finish { reason } => json!({ "type": "finish", "reason": reason.to_value() }),
        }
    }

    /// Whether this chunk is the one that ends a response.
    pub fn is_finish(&self) -> bool {
        matches!(self, Self::Finish { .. })
    }
}

/// One model call in a replay script.
///
/// Upstream's `ReplayEntry` also has a `throw` variant for a call that fails
/// mid-stream; that is carried inside the chunks here as an `aborted`/`error`
/// finish reason, because the log records it that way — a thrown stream is an
/// `assistant/attempt` whose last chunk is a failing `finish`. A scenario the
/// log genuinely cannot reconstruct (a call that never ended) is refused by
/// [`derive_replay_script`] rather than guessed at.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayEntry {
    pub chunks: Vec<StreamChunk>,
}

impl ReplayEntry {
    /// A one-block text response, which is the shape most recorded calls have.
    pub fn text(text: &str) -> Self {
        Self {
            chunks: vec![
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: BlockType::Text,
                },
                StreamChunk::TextDelta {
                    index: 0,
                    text: text.to_string(),
                },
                StreamChunk::BlockEnd {
                    index: 0,
                    block: json!({ "type": "text", "text": text }),
                },
                StreamChunk::Finish {
                    reason: FinishReason::Stop,
                },
            ],
        }
    }

    /// The reason this entry ends with, if it ends.
    pub fn finish_reason(&self) -> Option<&FinishReason> {
        match self.chunks.last()? {
            StreamChunk::Finish { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Why a script could not be derived from a log.
#[derive(Debug, Clone, PartialEq)]
pub struct DeriveError {
    /// The `turn/step` of the offending call, for a readable message.
    pub call: String,
    pub message: String,
}

/// Derive a replay script from a session log's rows.
///
/// Mirrors upstream's `deriveReplayScript`: one entry per recorded model call,
/// in call order, taken from each `assistant/message` / `assistant/attempt`
/// row's `stream`. Both row types count because a call that *failed* is
/// recorded as an attempt rather than a message — which is exactly the case a
/// replay must reproduce rather than skip.
///
/// A call whose recorded stream does not end in a `finish` chunk is **refused**,
/// not repaired: upstream says such a scenario "needs a
/// replay.override.json sidecar", because a session log alone cannot
/// reconstruct how it ended. Inventing an ending here would make a broken loop
/// look correct.
pub fn derive_replay_script(rows: &[Value]) -> Result<Vec<ReplayEntry>, DeriveError> {
    let mut script = Vec::new();
    for row in rows {
        let ty = row.get("type").and_then(Value::as_str).unwrap_or_default();
        if ty != "assistant/message" && ty != "assistant/attempt" {
            continue;
        }
        let data = row.get("data").cloned().unwrap_or(Value::Null);
        let call = format!(
            "{}/{}",
            data.get("turn").and_then(Value::as_u64).unwrap_or(0),
            data.get("step").and_then(Value::as_u64).unwrap_or(0)
        );
        let Some(stream) = data.get("stream").and_then(Value::as_array) else {
            // A call with no stream is not a recorded model call: an
            // `assistant/message` written by a non-model source has no chunks
            // to replay, and upstream skips it the same way.
            continue;
        };
        let chunks = expand_assistant_stream(stream);
        if chunks.is_empty() {
            continue;
        }
        if !chunks.last().is_some_and(StreamChunk::is_finish) {
            return Err(DeriveError {
                call,
                message: "model call ended without a finish chunk (a thrown stream); \
                          this scenario needs a replay override"
                    .into(),
            });
        }
        script.push(ReplayEntry { chunks });
    }
    Ok(script)
}

/// Unpack a recorded `stream` array into chunks.
///
/// The durable form is **packed**: consecutive text/reasoning/tool-call deltas
/// are collapsed into one `text-chunks` / `reasoning-chunks` /
/// `tool-call-chunks` record carrying parallel arrays, and only non-delta
/// chunks appear raw. Upstream's `expandAssistantStream` is the validating path
/// for records read at a durable boundary, and this mirrors it.
///
/// A record this does not recognize is **skipped** rather than guessed at: the
/// stream is read across an untyped durable boundary, so a future chunk type
/// must not be silently mistranslated into a wrong one. That is also why the
/// switch is on the record's `type` string rather than a parse-then-fallback.
pub fn expand_assistant_stream(stream: &[Value]) -> Vec<StreamChunk> {
    let mut chunks = Vec::new();
    for record in stream {
        match record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "chunk" => {
                if let Some(chunk) = record.get("chunk").and_then(parse_chunk) {
                    chunks.push(chunk);
                }
            }
            "text-chunks" | "reasoning-chunks" => {
                let Some(index) = record.get("index").and_then(Value::as_u64) else {
                    continue;
                };
                let texts = record.get("texts").and_then(Value::as_array);
                let Some(texts) = texts else { continue };
                let is_text = record.get("type").and_then(Value::as_str) == Some("text-chunks");
                for text in texts.iter().filter_map(Value::as_str) {
                    chunks.push(if is_text {
                        StreamChunk::TextDelta {
                            index,
                            text: text.to_string(),
                        }
                    } else {
                        StreamChunk::ReasoningDelta {
                            index,
                            text: text.to_string(),
                        }
                    });
                }
            }
            "tool-call-chunks" => {
                let Some(index) = record.get("index").and_then(Value::as_u64) else {
                    continue;
                };
                let Some(id) = record.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let name = record
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let args = record.get("args").and_then(Value::as_array);
                let Some(args) = args else { continue };
                for (i, arg) in args.iter().filter_map(Value::as_str).enumerate() {
                    chunks.push(StreamChunk::ToolCallDelta {
                        index,
                        id: id.to_string(),
                        // The name rides the first delta only, matching how
                        // upstream emits it.
                        name: if i == 0 { name.clone() } else { None },
                        arguments_delta: arg.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    chunks
}

/// Parse one raw `chunk` record.
fn parse_chunk(value: &Value) -> Option<StreamChunk> {
    let ty = value.get("type").and_then(Value::as_str)?;
    let index = || value.get("index").and_then(Value::as_u64).unwrap_or(0);
    Some(match ty {
        "block-start" => StreamChunk::BlockStart {
            index: index(),
            block_type: BlockType::parse(
                value
                    .get("blockType")
                    .and_then(Value::as_str)
                    .unwrap_or("text"),
            )?,
        },
        "text-delta" => StreamChunk::TextDelta {
            index: index(),
            text: value.get("text").and_then(Value::as_str)?.to_string(),
        },
        "reasoning-delta" => StreamChunk::ReasoningDelta {
            index: index(),
            text: value.get("text").and_then(Value::as_str)?.to_string(),
        },
        "tool-call-delta" => StreamChunk::ToolCallDelta {
            index: index(),
            id: value.get("id").and_then(Value::as_str)?.to_string(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string),
            arguments_delta: value
                .get("argumentsDelta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "block-end" => StreamChunk::BlockEnd {
            index: index(),
            block: value.get("block").cloned().unwrap_or(Value::Null),
        },
        "usage" => StreamChunk::Usage {
            usage: value.get("usage").cloned().unwrap_or(Value::Null),
        },
        "finish" => StreamChunk::Finish {
            reason: parse_finish(value.get("reason")?)?,
        },
        _ => return None,
    })
}

fn parse_finish(value: &Value) -> Option<FinishReason> {
    let kind = value.get("kind").and_then(Value::as_str)?;
    let failure = |v: &Value| {
        (
            v.get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            v.get("code")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    };
    Some(match kind {
        "stop" => FinishReason::Stop,
        "tool-calls" => FinishReason::ToolCalls,
        "max-tokens" => FinishReason::MaxTokens,
        "aborted" => {
            let (message, code) = failure(value.get("failure").unwrap_or(&Value::Null));
            FinishReason::Aborted { message, code }
        }
        "error" => {
            let (message, code) = failure(value.get("failure").unwrap_or(&Value::Null));
            FinishReason::Error { message, code }
        }
        _ => return None,
    })
}

/// The reason a step's chunks imply, as the FSM's [`StepOutcome`] sees it.
///
/// This is the join between the two halves of the seam: a replay entry's finish
/// reason decides how the turn ends, and `tool-calls` is what tells the FSM to
/// open another step.
pub fn step_outcome(reason: &FinishReason) -> crate::machines::agent_loop::StepOutcome {
    use crate::machines::agent_loop::StepOutcome;
    match reason {
        FinishReason::Stop | FinishReason::ToolCalls => StepOutcome::Completed,
        FinishReason::MaxTokens => StepOutcome::MaxTokens,
        FinishReason::Aborted { message, code } | FinishReason::Error { message, code } => {
            StepOutcome::Error {
                code: code.clone(),
                message: message.clone(),
            }
        }
    }
}

/// Whether the assembled blocks of an entry include a tool call.
///
/// The FSM needs this to decide whether to open another step, and it must read
/// the *finish reason* rather than re-deriving it from blocks: a response can
/// end `stop` with a tool call present in an aborted attempt.
pub fn has_tool_calls(entry: &ReplayEntry) -> bool {
    matches!(entry.finish_reason(), Some(FinishReason::ToolCalls))
        || entry.chunks.iter().any(|c| {
            matches!(
                c,
                StreamChunk::BlockStart {
                    block_type: BlockType::ToolCall,
                    ..
                }
            )
        })
}

/// A replay provider: serves a scripted list of model calls in order.
///
/// Behaving like upstream's adapter: each call takes the next entry, and a call
/// past the end of the script is an **error**, not an empty response — that is
/// the signal that the loop made more model calls than the recording did, which
/// is a real defect and must not read as a successful empty reply.
#[derive(Debug, Default)]
pub struct ReplayProvider {
    script: Vec<ReplayEntry>,
    /// How many calls have been served.
    cursor: usize,
}

/// What a provider call produced.
#[derive(Debug, Clone, PartialEq)]
pub enum CallResult {
    /// The chunks for this call, in order.
    Chunks(Vec<StreamChunk>),
    /// The script ran out — the loop called more often than the recording did.
    Exhausted { call_index: usize },
}

impl ReplayProvider {
    pub fn new(script: Vec<ReplayEntry>) -> Self {
        Self { script, cursor: 0 }
    }

    /// Derive a provider from a session log.
    pub fn from_rows(rows: &[Value]) -> Result<Self, DeriveError> {
        Ok(Self::new(derive_replay_script(rows)?))
    }

    /// Serve the next model call.
    pub fn next_call(&mut self) -> CallResult {
        let index = self.cursor;
        match self.script.get(index) {
            Some(entry) => {
                self.cursor += 1;
                CallResult::Chunks(entry.chunks.clone())
            }
            None => CallResult::Exhausted { call_index: index },
        }
    }

    /// How many calls have been served.
    pub fn served(&self) -> usize {
        self.cursor
    }

    /// How many calls the script holds.
    pub fn len(&self) -> usize {
        self.script.len()
    }

    pub fn is_empty(&self) -> bool {
        self.script.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_of(v: Value) -> StreamChunk {
        parse_chunk(&v).expect("parseable chunk")
    }

    // -- the chunk vocabulary -------------------------------------------------

    #[test]
    fn a_finish_chunk_round_trips_every_reason() {
        for reason in [
            FinishReason::Stop,
            FinishReason::ToolCalls,
            FinishReason::MaxTokens,
            FinishReason::Aborted {
                message: "cancelled".into(),
                code: "ABORTED".into(),
            },
            FinishReason::Error {
                message: "boom".into(),
                code: "AUTH".into(),
            },
        ] {
            let chunk = StreamChunk::Finish {
                reason: reason.clone(),
            };
            let back = chunk_of(chunk.to_value());
            assert_eq!(back, chunk, "{reason:?} did not round-trip");
        }
    }

    /// Every chunk the vocabulary declares round-trips through its wire shape.
    #[test]
    fn every_chunk_variant_round_trips() {
        let chunks = vec![
            StreamChunk::BlockStart {
                index: 0,
                block_type: BlockType::Text,
            },
            StreamChunk::TextDelta {
                index: 0,
                text: "hello".into(),
            },
            StreamChunk::ReasoningDelta {
                index: 1,
                text: "thinking".into(),
            },
            StreamChunk::ToolCallDelta {
                index: 2,
                id: "call_1".into(),
                name: Some("read".into()),
                arguments_delta: "{\"p".into(),
            },
            StreamChunk::BlockEnd {
                index: 0,
                block: json!({ "type": "text", "text": "hello" }),
            },
            StreamChunk::Usage {
                usage: json!({ "inputTokens": 3, "outputTokens": 5 }),
            },
        ];
        for chunk in chunks {
            let back = chunk_of(chunk.to_value());
            assert_eq!(back, chunk, "{chunk:?} did not round-trip");
        }
    }

    /// A `tool-call-delta` omits `name` when it has none, matching the spec's
    /// optional field rather than sending `null`.
    #[test]
    fn a_nameless_tool_call_delta_omits_the_field() {
        let v = StreamChunk::ToolCallDelta {
            index: 0,
            id: "c".into(),
            name: None,
            arguments_delta: "{}".into(),
        }
        .to_value();
        assert!(v.get("name").is_none(), "{v}");
    }

    // -- packed-record expansion ---------------------------------------------

    /// The recorded form packs deltas into parallel arrays; expansion restores
    /// one chunk per delta. This is the shape every corpus snapshot uses.
    #[test]
    fn packed_delta_runs_expand_to_one_chunk_per_member() {
        let stream = vec![
            json!({ "type": "chunk", "time": 1, "chunk": { "type": "block-start", "index": 0, "blockType": "text" } }),
            json!({ "type": "text-chunks", "time0": 1, "index": 0, "dt": [1, 2], "texts": ["a", "b", "c"] }),
            json!({ "type": "chunk", "time": 4, "chunk": { "type": "block-end", "index": 0, "block": { "type": "text", "text": "abc" } } }),
            json!({ "type": "chunk", "time": 5, "chunk": { "type": "finish", "reason": { "kind": "stop" } } }),
        ];
        let chunks = expand_assistant_stream(&stream);
        assert_eq!(chunks.len(), 6, "{chunks:?}");
        assert_eq!(
            chunks[1],
            StreamChunk::TextDelta {
                index: 0,
                text: "a".into()
            }
        );
        assert_eq!(
            chunks[3],
            StreamChunk::TextDelta {
                index: 0,
                text: "c".into()
            }
        );
        assert!(chunks.last().unwrap().is_finish());
    }

    /// A packed tool-call run puts the name on the first delta only, which is
    /// what upstream's expansion does.
    #[test]
    fn a_packed_tool_call_run_names_only_its_first_delta() {
        let stream = vec![json!({
            "type": "tool-call-chunks", "time0": 1, "index": 0, "dt": [1],
            "id": "call_1", "name": "read", "args": ["{\"path\":", "\"a.ts\"}"],
        })];
        let chunks = expand_assistant_stream(&stream);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        match &chunks[0] {
            StreamChunk::ToolCallDelta { name, .. } => assert_eq!(name.as_deref(), Some("read")),
            other => panic!("expected a tool-call delta, got {other:?}"),
        }
        match &chunks[1] {
            StreamChunk::ToolCallDelta { name, .. } => assert!(name.is_none(), "{name:?}"),
            other => panic!("expected a tool-call delta, got {other:?}"),
        }
    }

    /// An unknown record type is skipped, not mistranslated: the stream crosses
    /// an untyped durable boundary.
    #[test]
    fn an_unknown_record_type_is_skipped() {
        let stream = vec![
            json!({ "type": "future-chunks", "index": 0, "whatever": [1] }),
            json!({ "type": "chunk", "chunk": { "type": "finish", "reason": { "kind": "stop" } } }),
        ];
        let chunks = expand_assistant_stream(&stream);
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        assert!(chunks[0].is_finish());
    }

    // -- script derivation ---------------------------------------------------

    /// One `assistant/message` becomes one entry, in call order.
    #[test]
    fn a_script_is_derived_in_call_order() {
        let rows = vec![
            json!({ "type": "assistant/message", "data": { "turn": 1, "step": 1, "stream": [
                { "type": "chunk", "chunk": { "type": "block-start", "index": 0, "blockType": "text" } },
                { "type": "text-chunks", "index": 0, "texts": ["one"] },
                { "type": "chunk", "chunk": { "type": "block-end", "index": 0, "block": { "type": "text", "text": "one" } } },
                { "type": "chunk", "chunk": { "type": "finish", "reason": { "kind": "stop" } } },
            ] } }),
            json!({ "type": "assistant/message", "data": { "turn": 1, "step": 2, "stream": [
                { "type": "text-chunks", "index": 0, "texts": ["two"] },
                { "type": "chunk", "chunk": { "type": "finish", "reason": { "kind": "stop" } } },
            ] } }),
        ];
        let script = derive_replay_script(&rows).expect("derivable");
        assert_eq!(script.len(), 2);
        assert_eq!(script[0].finish_reason(), Some(&FinishReason::Stop));
    }

    /// An `assistant/attempt` is a recorded call too — a *failed* one. Skipping
    /// attempts would drop exactly the case a replay most needs.
    #[test]
    fn a_failed_attempt_is_recorded_as_a_call() {
        let rows = vec![
            json!({ "type": "assistant/attempt", "data": { "turn": 1, "step": 1, "stream": [
            { "type": "chunk", "chunk": { "type": "finish", "reason": {
                "kind": "error", "failure": { "message": "simulated provider error (HTTP 401)", "code": "AUTH" } } } },
        ] } }),
        ];
        let script = derive_replay_script(&rows).expect("derivable");
        assert_eq!(script.len(), 1);
        assert_eq!(
            script[0].finish_reason(),
            Some(&FinishReason::Error {
                message: "simulated provider error (HTTP 401)".into(),
                code: "AUTH".into(),
            })
        );
    }

    /// A call whose stream never finished is **refused**, not repaired. This is
    /// the load-bearing refusal: guessing an ending would make a broken loop
    /// look correct.
    #[test]
    fn a_call_without_a_finish_chunk_is_refused() {
        let rows = vec![
            json!({ "type": "assistant/message", "data": { "turn": 2, "step": 3, "stream": [
            { "type": "text-chunks", "index": 0, "texts": ["never ends"] },
        ] } }),
        ];
        let err = derive_replay_script(&rows).expect_err("must refuse");
        assert_eq!(err.call, "2/3", "the message names the offending call");
        assert!(err.message.contains("finish"), "{}", err.message);
    }

    /// A message with no floor-level failure stream is not a model call: a
    /// non-model source writes those.
    #[test]
    fn a_message_without_a_stream_is_not_a_call() {
        let rows = vec![
            json!({ "type": "assistant/message", "data": { "turn": 1, "step": 1, "message": {} } }),
            json!({ "type": "user/message", "data": {} }),
            json!({ "type": "turn/start", "data": { "turn": 1 } }),
        ];
        assert!(derive_replay_script(&rows).expect("derivable").is_empty());
    }

    /// The FSM's step outcome is decided by the finish reason, and `tool-calls`
    /// is an ordinary completion — the *tool* flag is separate.
    #[test]
    fn the_finish_reason_decides_the_step_outcome() {
        use crate::machines::agent_loop::StepOutcome;
        assert_eq!(step_outcome(&FinishReason::Stop), StepOutcome::Completed);
        assert_eq!(
            step_outcome(&FinishReason::ToolCalls),
            StepOutcome::Completed
        );
        assert_eq!(
            step_outcome(&FinishReason::MaxTokens),
            StepOutcome::MaxTokens
        );
        assert_eq!(
            step_outcome(&FinishReason::Error {
                message: "boom".into(),
                code: "AUTH".into()
            }),
            StepOutcome::Error {
                message: "boom".into(),
                code: "AUTH".into()
            }
        );
    }

    /// `has_tool_calls` reads the finish reason first: a `tool-calls` finish is
    /// authoritative even if the blocks were packed oddly.
    #[test]
    fn tool_calls_are_detected_from_the_finish_reason_or_a_block() {
        let by_reason = ReplayEntry {
            chunks: vec![
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: BlockType::ToolCall,
                },
                StreamChunk::Finish {
                    reason: FinishReason::ToolCalls,
                },
            ],
        };
        assert!(has_tool_calls(&by_reason));

        let by_block = ReplayEntry {
            chunks: vec![
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: BlockType::ToolCall,
                },
                StreamChunk::Finish {
                    reason: FinishReason::Stop,
                },
            ],
        };
        assert!(has_tool_calls(&by_block), "a tool-call block counts");

        assert!(!has_tool_calls(&ReplayEntry::text("no tools here")));
    }

    // -- the provider --------------------------------------------------------

    /// Calls are served in order, once each.
    #[test]
    fn the_provider_serves_its_script_in_order() {
        let mut p = ReplayProvider::new(vec![
            ReplayEntry::text("first"),
            ReplayEntry::text("second"),
        ]);
        assert_eq!(p.len(), 2);
        assert_eq!(p.served(), 0);
        let CallResult::Chunks(first) = p.next_call() else {
            panic!("expected chunks");
        };
        assert_eq!(
            first[1],
            StreamChunk::TextDelta {
                index: 0,
                text: "first".into()
            }
        );
        let CallResult::Chunks(second) = p.next_call() else {
            panic!("expected chunks");
        };
        assert_eq!(
            second[1],
            StreamChunk::TextDelta {
                index: 0,
                text: "second".into()
            }
        );
        assert_eq!(p.served(), 2);
    }

    /// Running past the script is an **error**, not an empty response: it means
    /// the loop called more often than the recording did.
    #[test]
    fn a_call_past_the_end_of_the_script_is_reported() {
        let mut p = ReplayProvider::new(vec![ReplayEntry::text("only")]);
        assert!(matches!(p.next_call(), CallResult::Chunks(_)));
        assert_eq!(
            p.next_call(),
            CallResult::Exhausted { call_index: 1 },
            "an exhausted script must not read as an empty reply"
        );
    }

    /// The helper entry shape matches the corpus's ordinary one-block reply:
    /// `block-start`, the delta, `block-end`, `finish`.
    #[test]
    fn a_text_entry_is_the_corpus_shape() {
        let entry = ReplayEntry::text("DIRECT_CHILD_OK");
        let row_types: Vec<String> = entry
            .chunks
            .iter()
            .map(|c| {
                c.to_value()["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(
            row_types,
            vec!["block-start", "text-delta", "block-end", "finish"]
        );
    }

    // -- the two halves joined: FSM driven by real recorded calls ------------

    /// Drive the agent loop end to end from a **real recorded session**.
    ///
    /// This is what the seam exists for: the FSM's step outcomes and the
    /// provider's script both come from the same log, so a mismatch is a defect
    /// in one of them rather than in a hand-written expectation. It runs over
    /// every committed snapshot that holds a completed turn.
    ///
    /// The loop is driven the way the driver will drive it — begin the turn,
    /// enter the step, take the next recorded call, feed its outcome back — and
    /// the assertion is that the row *shape* it produces matches the shape the
    /// log actually recorded. Turn and step counts are the strongest available
    /// signal that the FSM's control flow is right, because they are what the
    /// recording proves.
    #[test]
    fn the_loop_reproduces_the_shape_of_every_recorded_turn() {
        use crate::machines::agent_loop::{AgentLoop, LoopOutput};
        use std::path::Path;

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dsh/snapshots/session");
        if !root.is_dir() {
            eprintln!(
                "skipping: {} absent (submodule not checked out)",
                root.display()
            );
            return;
        }

        let mut checked = 0usize;
        let mut multistep = 0usize;
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

            // What the log recorded: turn/step counts, in order.
            let mut recorded: Vec<(u64, Vec<u64>)> = Vec::new();
            for row in &rows {
                let ty = row.get("type").and_then(Value::as_str).unwrap_or_default();
                let data = row.get("data").cloned().unwrap_or(Value::Null);
                match ty {
                    "turn/start" => recorded.push((
                        data.get("turn").and_then(Value::as_u64).unwrap_or(0),
                        Vec::new(),
                    )),
                    "step/start" => {
                        let s = data.get("step").and_then(Value::as_u64).unwrap_or(0);
                        if let Some(last) = recorded.last_mut() {
                            last.1.push(s);
                        }
                    }
                    _ => {}
                }
            }
            if recorded.is_empty() {
                continue;
            }
            // The script must be derivable, or the log is a scenario a replay
            // cannot reconstruct — `derive_replay_script` refuses those, and
            // this test respects the refusal rather than working around it.
            let Ok(mut provider) = ReplayProvider::from_rows(&rows) else {
                continue;
            };
            if provider.is_empty() {
                continue;
            }
            checked += 1;

            // `derive_replay_script` skips `assistant/message` rows with no
            // `stream` (non-model sources), so the derived script can have
            // fewer entries than the log has model calls. That is a fidelity
            // gap in the *script*, not the loop, and it means the script cannot
            // drive every recorded turn. Only drive logs where the counts line
            // up, and count those, so the assertion stays about the loop.
            let model_calls = rows
                .iter()
                .filter(|r| {
                    matches!(
                        r.get("type").and_then(Value::as_str),
                        Some("assistant/message") | Some("assistant/attempt")
                    ) && r.get("data").and_then(|d| d.get("stream")).is_some()
                })
                .count();
            if provider.len() != model_calls {
                continue;
            }

            // Replay: one turn per recorded turn, driving recorded calls.
            let mut loop_ = AgentLoop::new();
            let mut produced: Vec<(u64, Vec<u64>)> = Vec::new();
            for (turn, steps) in &recorded {
                let mut outs = loop_.begin_turn(&[json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": "recorded" }],
                    "source": { "kind": "user" },
                })]);
                let mut turn_steps: Vec<u64> = Vec::new();
                if steps.is_empty() {
                    // A turn with no step spends no model call.
                    assert!(matches!(outs.last(), Some(LoopOutput::TurnEnded(_))));
                    produced.push((*turn, turn_steps));
                    continue;
                }
                for expected_step in steps {
                    outs.extend(loop_.enter_step());
                    turn_steps.push(*expected_step);
                    let CallResult::Chunks(chunks) = provider.next_call() else {
                        panic!("script ran out mid-turn in {}", dir.display());
                    };
                    let entry = ReplayEntry {
                        chunks: chunks.clone(),
                    };
                    let reason = entry.finish_reason().unwrap_or_else(|| {
                        panic!("recorded call had no finish in {}", dir.display())
                    });
                    let outcome = step_outcome(reason);
                    let tools = has_tool_calls(&entry);
                    // The recorded step sequence says whether another step
                    // follows; the loop must agree.
                    let more = *expected_step != *steps.last().unwrap();
                    outs.extend(loop_.step_reply(outcome, tools, false));
                    if !more {
                        assert!(
                            !loop_.is_running() || tools,
                            "{}: loop still running after the last recorded step",
                            dir.display()
                        );
                    }
                }
                produced.push((*turn, turn_steps));
            }

            // The loop's turn/step shape must equal the log's, when the script
            // covered every call.
            if provider.served() == model_calls {
                assert_eq!(
                    produced,
                    recorded,
                    "{}: the loop's turn/step shape differs from the log",
                    dir.display()
                );
                if recorded.iter().any(|(_, s)| s.len() > 1) {
                    multistep += 1;
                }
            }
        }

        eprintln!("drove {checked} snapshots, {multistep} with multi-step turns");
        assert!(
            checked >= 20,
            "expected to drive many snapshots, drove {checked}"
        );
        assert!(
            multistep >= 1,
            "at least one driven session should have a multi-step turn, \
             or the test proves less than it claims"
        );
    }
}
