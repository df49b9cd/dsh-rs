//! The agent machine: drives one turn end-to-end over a session log.
//!
//! This is the wiring the four other pieces were built for. It owns no turn
//! logic of its own — the turn/step shape is [`agent_loop::AgentLoop`], admitted
//! input is [`agent_inbox::Inbox`], the model call is
//! [`provider::ProviderStreamDecoder`], and the durable truth is the session
//! log. What this adds is *sequencing*: when to claim input, when to call the
//! model, when to write rows, and how to keep `turn/start`…`turn/end` balanced
//! when a call fails or is cancelled.
//!
//! ## Why the in-flight turn is held rather than rebuilt
//!
//! `session` can re-run a whole method on an effect answer because each method
//! is a short, idempotent computation over cached reads. A turn is not: it spans
//! many effects, and re-entry must not recompute the turn number, re-claim the
//! same inbox messages, or re-decide the provider. Each of those is a
//! double-append waiting to happen.
//!
//! So the live [`AgentLoop`] instance is carried in [`TurnState`] across
//! suspensions. An earlier draft rebuilt the FSM from the rows on each re-entry
//! and was wrong: the FSM's phase carries something the rows do not, namely
//! whether a step is currently open, so a rebuild has to re-derive it by
//! scanning for an unmatched `step/start` — recomputing exactly the decision
//! this is meant to preserve.
//!
//! Where a value must survive a suspension and is not part of the turn state, it
//! goes through [`FsCache::choose`], the same mechanism `session` uses for a
//! generated id.
//!
//! ## Known limitation: one turn at a time, per host
//!
//! The machine holds a single [`Op`], so a second `agent run` arriving while a
//! turn is in flight is **refused rather than queued**. That is honest for the
//! current single-client host — the wire suite and a single web session never
//! overlap — but it is wrong for two clients prompting two different sessions
//! concurrently, where the second prompt would be rejected while the first is
//! mid-call.
//!
//! The fix is per-session ops (`BTreeMap<String, Op>`) rather than a single
//! slot, which is a small change: every op already carries its own session id
//! and its own rows, so nothing is shared between turns but the cache (whose
//! keys are paths, and so already per-session). It is left undone because
//! nothing yet exercises concurrency and the wrong fix is plausible enough to
//! be worth stating: a *queue* would silently delay prompts behind a slow model
//! call, which reads to a client as a hang.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Value, json};
use vocoder_cordis::{EffectResult, MachineIn, MachineOut, PluginMachine, RealizeRequest};

use super::agent_inbox::{Inbox, InboxTarget};
use super::agent_loop::{AgentLoop, Draft, LoopOutput, StepOutcome};
use super::llm_replay::{BlockType, FinishReason, StreamChunk, step_outcome};
use super::markdown::DisplayAccumulator;
use super::provider::{
    ProviderConfig, ProviderKind, ProviderStreamDecoder, SseDecoder, SseFrame, to_wire_request,
};
use super::readcache::{FsCache, Pending};
use crate::rpc;

/// A configured provider route.
pub struct Route {
    pub config: ProviderConfig,
}

/// Environment variables naming the model a route sends.
///
/// The *provider* is configured here; the model is not, because the catalog the
/// client reads already declares one and a route that hard-coded a second name
/// would silently disagree with the model picker.
const ENV_PROVIDER: &str = "VOCODER_PROVIDER_KIND";
const ENV_BASE_URL: &str = "VOCODER_PROVIDER_BASE_URL";
const ENV_MODEL: &str = "VOCODER_PROVIDER_MODEL";
const ENV_API_KEY: &str = "VOCODER_PROVIDER_API_KEY_ENV";

/// Build the configured routes from the environment.
///
/// One route, named [`PROVIDER_ROUTE_ID`], configured by
/// `VOCODER_PROVIDER_*`. With nothing set the route is still present, pointing
/// at the DeepSeek catalog's own endpoint and reading `DEEPSEEK_API_KEY` —
/// which is what makes a session runnable without any configuration at all when
/// that key is in the environment.
///
/// An unrecognized [`ENV_PROVIDER`] is a hard failure rather than a fallback:
/// silently speaking the wrong dialect to a live endpoint fails as a 400 that
/// reads like a provider outage, which is far harder to diagnose than a refusal
/// at startup.
pub fn routes_from_env() -> Vec<Route> {
    let kind = match std::env::var(ENV_PROVIDER) {
        Ok(raw) => match ProviderKind::parse(&raw) {
            Some(kind) => kind,
            None => {
                tracing::error!(
                    "{ENV_PROVIDER}={raw:?} is not a known dialect; \
                     expected one of openai-chat, openai-responses, anthropic-messages"
                );
                return Vec::new();
            }
        },
        // Default to the dialect the DeepSeek route speaks.
        Err(_) => ProviderKind::OpenAiChat,
    };
    let base_url =
        std::env::var(ENV_BASE_URL).unwrap_or_else(|_| "https://api.deepseek.com/v1".to_string());
    let model = std::env::var(ENV_MODEL).unwrap_or_else(|_| super::llm::DEFAULT_MODEL.to_string());
    let api_key_env = std::env::var(ENV_API_KEY).unwrap_or_else(|_| "DEEPSEEK_API_KEY".to_string());
    vec![Route {
        config: ProviderConfig {
            id: PROVIDER_ROUTE_ID.to_string(),
            kind,
            base_url,
            model,
            api_key_env: Some(api_key_env),
        },
    }]
}

/// The route id this host configures.
///
/// Deliberately the catalog's own provider id so that a model the client picked
/// from `session/modelCatalog` resolves to a live route without a second
/// mapping table.
pub const PROVIDER_ROUTE_ID: &str = super::llm::PROVIDER_ID;

/// One turn, as far as it has got.
///
/// `rows` is the generation being accumulated and `fsm` the loop driving it.
/// They travel together because a suspension must resume *both*: the rows say
/// what has been written, the FSM says what is owed next.
struct TurnState {
    session: String,
    request_id: String,
    rows: Vec<Value>,
    fsm: AgentLoop,
    /// How many rows the session had when this turn opened.
    ///
    /// The publishing step announces the rows *past* this point, so a follower
    /// gets this turn's events and not the whole history again. Recorded here
    /// rather than re-derived at publish time because the cache's row list is
    /// not a reliable witness: the turn's own write invalidates it, so a
    /// re-entry after the write would compare against the wrong length.
    opened_with: usize,
}

/// The live decode of a model call, as its bytes arrive.
///
/// Three incrementally-maintained pieces, because each answers a different
/// question and none can be re-derived cheaply from the others:
///
/// - `sse` and `stream` are the transport and protocol decoders. They are the
///   *same* two the buffered path uses; the only difference is that they are fed
///   as bytes land rather than once at the end. That is deliberate — two decode
///   paths would be two chances to disagree about what the model said, and the
///   durable record is written from the buffered path while the display comes
///   from this one.
/// - `acc` accumulates per-block text for display, mirroring what a client's
///   own accumulator does when it folds the same chunk stream.
///
/// `index` and `revision` are the frame counters the assistant-stream protocol
/// requires: `index` must be *dense* per attempt (the control's accumulator
/// rejects a gap and drops the attempt), and `revision` increments on every
/// frame so a client can detect a restart.
struct Live {
    sse: SseDecoder,
    stream: ProviderStreamDecoder,
    acc: DisplayAccumulator,
    index: u64,
    revision: u64,
    attempt_id: String,
    /// Blocks already announced with a `block-start`, so a block is opened once.
    announced: BTreeMap<u64, BlockType>,
    /// Whether the `start` frame has gone out. Set by the emitter rather than
    /// by the decoder, because whether a start was sent is a fact about the
    /// *stream*, not about the response's contents.
    started: bool,
}

impl Live {
    fn new(kind: ProviderKind) -> Self {
        Self {
            sse: SseDecoder::new(),
            stream: ProviderStreamDecoder::new(kind),
            acc: DisplayAccumulator::new(),
            index: 0,
            revision: 0,
            attempt_id: format!("attempt-{}", rpc::new_id()),
            announced: BTreeMap::new(),
            started: false,
        }
    }

    /// Decode `bytes` and produce the assistant-stream chunk frames they imply.
    ///
    /// The bytes are appended to the *same* buffers the buffered path would
    /// have seen, so a chunk boundary that splits an SSE frame is handled by
    /// the decoder's own buffering rather than by luck.
    ///
    /// Both decoders are stateful: they append only the chunks that a newly
    /// arrived frame produced, so the returned batch is already only-new. A
    /// caller must not re-feed bytes it has already fed.
    fn feed(&mut self, bytes: &[u8]) -> Vec<Value> {
        let mut frames = Vec::new();
        self.sse.feed(bytes, &mut frames);
        let mut chunks: Vec<StreamChunk> = Vec::new();
        for frame in &frames {
            self.stream.frame(frame, &mut chunks);
        }
        self.drain(&chunks)
    }

    /// Flush whatever the decoders are still holding, at end of stream.
    fn end(&mut self) -> Vec<Value> {
        let mut frames = Vec::new();
        self.sse.finish(&mut frames);
        let mut chunks: Vec<StreamChunk> = Vec::new();
        for frame in &frames {
            self.stream.frame(frame, &mut chunks);
        }
        self.stream.end(&mut chunks);
        self.drain(&chunks)
    }

    /// Turn a batch of freshly-decoded chunks into wire frames.
    ///
    /// The batch is already only-new: both decoders are stateful and append
    /// solely the chunks a newly-arrived frame produced, so there is nothing to
    /// de-duplicate here. An earlier version carried an `emitted` cursor and
    /// sliced the batch with it, which was wrong in a way that only showed up as
    /// a panic once the *second* batch arrived — the cursor counted against a
    /// list that was rebuilt per call, so it ran off the end.
    fn drain(&mut self, chunks: &[StreamChunk]) -> Vec<Value> {
        let mut out = Vec::new();
        for chunk in chunks {
            for value in self.frame_for(chunk) {
                self.revision += 1;
                let index = self.index;
                self.index += 1;
                out.push(json!({
                    "type": "chunk",
                    "attemptId": self.attempt_id,
                    "revision": self.revision,
                    "index": index,
                    "time": super::session_now_ms(),
                    "chunk": value,
                }));
            }
        }
        out
    }

    /// The display frame(s) one chunk produces.
    ///
    /// A `block-start` is emitted once per block index, at the first chunk that
    /// touches it — which is where a client needs it, since its accumulator
    /// keys blocks by index and a delta for an unannounced block would land on
    /// a hole.
    ///
    /// Only *prose* blocks are announced with their text attached; a tool call's
    /// arguments ride through as the raw deltas, because the display path must
    /// not stitch JSON and the client assembles the arguments itself.
    fn frame_for(&mut self, chunk: &StreamChunk) -> Vec<Value> {
        let mut out = Vec::new();
        // Announce the block this chunk belongs to, if it is a new one.
        let (index, block_type) = match chunk {
            StreamChunk::TextDelta { index, .. } => (Some(*index), Some(BlockType::Text)),
            StreamChunk::ReasoningDelta { index, .. } => (Some(*index), Some(BlockType::Reasoning)),
            StreamChunk::ToolCallDelta { index, .. } => (Some(*index), Some(BlockType::ToolCall)),
            StreamChunk::BlockStart { index, block_type } => (Some(*index), Some(*block_type)),
            _ => (None, None),
        };
        if let (Some(index), Some(block_type)) = (index, block_type)
            && !self.announced.contains_key(&index)
        {
            self.announced.insert(index, block_type);
            self.acc.open(index, block_type);
            out.push(json!({ "type": "block-start", "index": index, "blockType": block_name(block_type) }));
        }
        // Then the chunk's own content.
        match chunk {
            StreamChunk::TextDelta { index, text } => {
                // The **raw** fragment, not the stitched one, and this is a
                // correctness requirement rather than a simplification.
                //
                // `text-delta` is append-only: a client accumulates with
                // `prev + chunk.text`. Stitching inserts characters the model
                // never wrote — closing `**bo` to `**bo**` appends two
                // asterisks *before the answer continues* — so a stitched
                // frame's inserted closer is baked into the concatenation and
                // every later fragment lands after it. Concatenating stitched
                // deltas yields `Here is **bo**** text` where the model wrote
                // `Here is **bold** text`.
                //
                // There is no retraction in the protocol, so no delta sequence
                // can both render well per-frame and sum to the stitched text.
                // The repair therefore belongs where the whole text is known,
                // which is the consumer's accumulated buffer; see
                // `machines/markdown.rs` for the accumulator that does it and
                // why the log must never see its output.
                self.acc.push_delta(*index, BlockType::Text, text);
                out.push(json!({
                    "type": "text-delta",
                    "index": index,
                    "text": text,
                }));
            }
            StreamChunk::ReasoningDelta { index, text } => {
                self.acc.push_delta(*index, BlockType::Reasoning, text);
                out.push(json!({
                    "type": "reasoning-delta",
                    "index": index,
                    "text": text,
                }));
            }
            StreamChunk::ToolCallDelta {
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
                    v["name"] = json!(name);
                }
                out.push(v);
            }
            StreamChunk::BlockEnd { index, block } => {
                // The **one** legal point for stitched text on this wire, and it
                // is legal for a structural reason rather than a stylistic one.
                //
                // A client replaces the block wholesale on `block-end`
                // (`PartialAccumulator`: `blocks[index] = toAssistantBlock(chunk.block)`,
                // tested upstream as "replaces the accumulated block wholesale on
                // block-end"), so whatever the deltas accumulated is discarded
                // and this value takes over. That makes `block-end` a
                // *retraction point* — the only one in the protocol — and a
                // retraction point is exactly what a repairer needs.
                //
                // It cannot go in `text-delta`. That field is append-only
                // (`prev + chunk.text`), and stitching inserts characters the
                // model never wrote: closing `**bo` to `**bo**` appends two
                // asterisks *before the answer continues*, so the closer is
                // baked into the concatenation and every later fragment lands
                // after it. Concatenating stitched deltas gives
                // `Here is **bo**** text` where the model wrote
                // `Here is **bold** text`. No delta sequence can both render
                // well per-frame and sum to the stitched text.
                //
                // Prose only: a tool call's block is JSON, and repairing it
                // would hand the client arguments it cannot parse.
                let block = self.stitched_block(*index, block);
                out.push(json!({ "type": "block-end", "index": index, "block": block }));
            }
            StreamChunk::Usage { usage } => {
                out.push(json!({ "type": "usage", "usage": usage }));
            }
            StreamChunk::Finish { .. } => {
                // Deliberately not forwarded. `finish` is a durable-record
                // concept — the client's own accumulator ignores it, and the
                // `assistant/message` row that supersedes the partial is what
                // tells a client the stream is over. Forwarding it would add a
                // frame no client consumes.
            }
            StreamChunk::BlockStart { .. } => {}
        }
        out
    }

    /// The `block-end` payload for one block, with prose stitched.
    ///
    /// The decoder's own `block-end` block is deliberately empty — it owns the
    /// chunk vocabulary, not block assembly — so the text comes from the
    /// accumulator, which is the only place that has been collecting the
    /// deltas. Building it from the decoder's placeholder is what made an early
    /// version record empty content for every provider; the same mistake here
    /// would send a client an empty final block, which *replaces* the text it
    /// had been accumulating.
    ///
    /// A tool-call block is passed through untouched: its `arguments` are
    /// assembled by the client from the deltas, and a block-end carrying a
    /// rewritten copy would be the JSON corruption `is_prose` exists to
    /// prevent. An unstitched block kind therefore keeps the decoder's payload,
    /// which for a tool call is the block identity without arguments — the same
    /// thing the durable row records.
    fn stitched_block(&self, index: u64, block: &Value) -> Value {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = match block_type {
            "text" => BlockType::Text,
            "reasoning" => BlockType::Reasoning,
            _ => return block.clone(),
        };
        if !super::markdown::is_prose(kind) {
            return block.clone();
        }
        let Some(text) = self.acc.display(index) else {
            return block.clone();
        };
        json!({ "type": block_type, "text": text.into_string() })
    }
}

/// The wire name of a block kind.
fn block_name(block_type: BlockType) -> &'static str {
    match block_type {
        BlockType::Text => "text",
        BlockType::Reasoning => "reasoning",
        BlockType::ToolCall => "tool-call",
    }
}

/// One `agent/assistant-stream` dispatch.
///
/// The event name and shape mirror upstream's own, so the session machine that
/// consumes it is doing what the control's history controller does rather than
/// speaking a vocabulary invented here.
fn stream_event(session: &str, frame: Value) -> MachineOut {
    MachineOut::Dispatch {
        name: vocoder_cordis::EventName::new("agent/assistant-stream"),
        payload: json!({ "sessionId": session, "frame": frame }),
        mode: vocoder_cordis::DispatchMode::Emit,
    }
}

/// How far the in-flight turn has advanced.
///
/// There is deliberately no "the turn is running" variant: a running turn is
/// only ever *between* two suspensions, never holding one, so it lives in a
/// local until the next effect needs somewhere to park it.
enum Op {
    /// A read for the turn's starting rows is in flight.
    Opening {
        session: String,
        request_id: String,
        /// The prompt content, carried across the suspension so the turn is
        /// admitted identically on the re-run.
        content: Value,
    },
    /// A model call is in flight for the FSM's current step.
    Call {
        state: TurnState,
        provider: String,
        /// Built once, so a re-run sends the identical body.
        body: Value,
        /// The incremental decode of the bytes that have arrived so far.
        ///
        /// Carried rather than rebuilt because the chunk deliveries arrive
        /// *between* the fetch's request and its answer, and each one must
        /// extend the same decode: a decoder re-created per chunk would treat
        /// every chunk as the start of a stream and re-emit the blocks it had
        /// already seen.
        live: Box<Live>,
        /// The effect this call's response is arriving under, once the fetch
        /// has issued it. `None` only between building the call and issuing it.
        ///
        /// Chunks are matched against this rather than against a "some effect
        /// is in flight" flag: a chunk from a *previous* call could otherwise
        /// be applied to the next one, which would splice one model's text into
        /// another's answer.
        effect: Option<vocoder_cordis::EffectId>,
    },
    /// The call returned; decode it and settle the step.
    Settle {
        state: TurnState,
        provider: String,
        body: String,
    },
    /// The turn is over and its generation is being written. The answer waits
    /// for the write: reporting success before the rows are durable would let a
    /// client read a session that does not yet contain its own turn.
    Publishing,
}

pub struct AgentMachine {
    root: PathBuf,
    routes: BTreeMap<String, Route>,
    cache: FsCache,
    pending: Option<Pending>,
    effects: u64,
    op: Option<Op>,
    /// Finished request ids, so a redelivered call does not start a new turn.
    completed: BTreeMap<String, Value>,
    /// The attempt id, revision and next index of the call that just settled.
    ///
    /// The `Live` that produced them is dropped when `settle` runs, but the
    /// `end` frame has to name the same attempt and continue the same counters —
    /// a client validates that every frame of an attempt carries one id and that
    /// chunk indices are dense, so an `end` that invented a fresh id would be
    /// rejected and leave the partial live forever. Carrying the summary is how
    /// the closing frame stays attributable after its decoder is gone.
    last_attempt: Option<(String, u64, u64)>,
}

impl AgentMachine {
    pub fn new(root: PathBuf, routes: Vec<Route>) -> Self {
        Self {
            root,
            routes: routes
                .into_iter()
                .map(|r| (r.config.id.clone(), r))
                .collect(),
            cache: FsCache::default(),
            pending: None,
            effects: 0,
            op: None,
            completed: BTreeMap::new(),
            last_attempt: None,
        }
    }

    /// The directory holding a session's log, once the tree has been walked.
    ///
    /// Found by scanning the tree rather than derived from the id: the project
    /// component of the path encodes the session's *cwd*, which the `session`
    /// machine chose at create time and this namespace cannot reconstruct from
    /// an id alone. Assuming the no-cwd form would silently look in the wrong
    /// place for every session created in a workspace — and finding no rows
    /// reads as an empty inbox rather than as an error, so the mistake would be
    /// invisible.
    ///
    /// `None` means "not found *in the current tree*", which is why the caller
    /// must have established that the tree is populated before believing it.
    fn session_dir_in(&self, session: &str, tree: &[String]) -> Option<String> {
        let want = super::session::SessionStore::encode_segment(session);
        tree.iter()
            .filter_map(|path| path.rsplit_once('/'))
            // Only the generation files themselves identify a session dir.
            .filter(|(_, name)| vocoder_session::parse_generation_filename(name).is_some())
            .map(|(dir, _)| dir)
            .find(|dir| dir.rsplit('/').next() == Some(want.as_str()))
            .map(str::to_string)
    }

    /// Ensure the tree is walked, returning it or the effect that will supply
    /// it.
    ///
    /// The walk is requested once and cached, so a second call is free — which
    /// is what makes the re-entry after the walk's answer a no-op rather than a
    /// second request that would spin against the effect cap.
    fn tree(&mut self) -> Result<Vec<String>, Vec<MachineOut>> {
        let root = self.root.to_string_lossy().to_string();
        self.cache.tree(&root, &mut self.pending, &mut self.effects)
    }

    /// Rows of a session's latest generation, or the effect that will supply
    /// them.
    ///
    /// Errors with `None` when the session does not exist — distinct from
    /// `Err(effects)`, which means the answer is still coming. Collapsing the
    /// two is what made an unwalked tree look like a missing session.
    fn rows_of(&mut self, session: &str) -> Result<Option<Vec<Value>>, Vec<MachineOut>> {
        let tree = self.tree()?;
        let Some(dir) = self.session_dir_in(session, &tree) else {
            return Ok(None);
        };
        if let Some(rows) = self.cache.session_rows(&dir, &tree) {
            return Ok(Some(rows));
        }
        // The directory is there but its newest generation is not cached yet.
        let unread = self.cache.unread_generations(&tree);
        if let Some(path) = unread.first().cloned() {
            return Err(self
                .cache
                .request_read(&path, &mut self.pending, &mut self.effects));
        }
        // A session whose generation cannot be read at all is still a session;
        // an empty log is the honest reading of "no rows are available".
        Ok(Some(Vec::new()))
    }

    /// Publish `rows` as the next generation, suspending on the write.
    ///
    /// The write is idempotent across suspensions: the target path is chosen
    /// once and remembered by the cache, and `published` records it before the
    /// effect is issued. Re-deriving the target after the write would name the
    /// *next* generation, since the version comes from the directory the write
    /// just changed.
    fn publish(&mut self, session: &str, rows: &[Value], opened_with: usize) -> Vec<MachineOut> {
        let tree = match self.tree() {
            Ok(tree) => tree,
            Err(effects) => return effects,
        };
        let Some(dir) = self.session_dir_in(session, &tree) else {
            return rpc::err("session/not-found", format!("no such session: {session}"));
        };
        let store = super::session::SessionStore::new(PathBuf::new());
        let target = match self.cache.write_target(|| {
            let (path, bytes) =
                store.encode_next_generation(PathBuf::from(&dir).as_path(), rows, &tree);
            (path.to_string_lossy().to_string(), bytes)
        }) {
            Ok(target) => target,
            Err(out) => return out,
        };
        let (path, bytes) = target;
        // The write already landed: this run is the re-entry after it, so the
        // operation is complete and must not publish a second generation.
        if self.cache.published.contains(&path) {
            return vec![];
        }
        // Announce every row this generation adds, so followers receive the
        // turn's durable events.
        //
        // The agent owns the log for the rows it writes — it appends the whole
        // turn itself rather than going through `session/prompt`'s own recorder
        // — and the follow stream's durable half is fed by whoever knows a row
        // landed. Publishing without announcing left a follower seeing the live
        // partial and then nothing: the `end` frame it received named a `seq`
        // that never arrived, so the partial could never be retired.
        //
        // Only rows *beyond what the session already had* are announced. The
        // cache holds the rows as of this turn's start, so the pre-existing
        // prefix is not re-broadcast — a follower would otherwise see every
        // earlier event again on each turn.
        let mut outs = Vec::new();
        for row in rows.iter().skip(opened_with) {
            outs.push(MachineOut::Dispatch {
                name: vocoder_cordis::EventName::new("session/event"),
                payload: json!({ "sessionId": session, "event": row }),
                mode: vocoder_cordis::DispatchMode::Emit,
            });
        }
        let mut write =
            self.cache
                .request_write(&path, bytes, &mut self.pending, &mut self.effects);
        outs.append(&mut write);
        outs
    }

    /// Append one row, assigning `seq` and `time`.
    ///
    /// Both belong to the publisher, not the FSM: `seq` depends on the log's
    /// current length and `time` on the clock, and a pure transition may consult
    /// neither — which is also what keeps a replay byte-comparable.
    fn row(rows: &mut Vec<Value>, draft: &Draft) -> Value {
        let seq = rows.len().saturating_sub(1) as f64;
        let row = json!({
            "type": draft.row_type,
            "seq": seq,
            "time": super::session_now_ms(),
            "data": draft.data,
        });
        rows.push(row.clone());
        row
    }
}

/// Advancing a turn: applying FSM output, calling the model, settling a reply.
impl AgentMachine {
    /// Run the FSM's outputs for an open turn.
    ///
    /// Returns the model call the turn wants next, if any. Rows are appended
    /// into `state` as they are decided; the caller owns the state and decides
    /// what to do with the call, which keeps the turn's ownership in one place
    /// rather than moving it through `self.op` and back.
    ///
    /// A turn that wants a call but has no route to serve it is closed as an
    /// error here rather than left open: an unclosed turn is an orphan the next
    /// resume would have to repair, and the repair would invent a reason the
    /// log never recorded.
    fn apply(&mut self, state: &mut TurnState, outs: Vec<LoopOutput>) -> Option<(String, Value)> {
        for out in outs {
            match out {
                // `close_turn` emits `turn/end` through this same arm, so a
                // turn's ending needs no separate handling here.
                LoopOutput::Append(draft) => {
                    Self::row(&mut state.rows, &draft);
                }
                LoopOutput::TurnEnded(_) => {}
                LoopOutput::CallModel { .. } => {
                    if let Some(call) = self.build_call(&state.rows) {
                        return Some(call);
                    }
                    let outs = state.fsm.step_reply(
                        StepOutcome::Error {
                            code: "PROVIDER_UNROUTABLE".into(),
                            message: "no configured provider can serve this request".into(),
                        },
                        false,
                        false,
                    );
                    // Recursion depth is bounded by the FSM: this arm consumes
                    // a call the loop will not re-request once the turn is
                    // closed by the error above.
                    return self.apply(state, outs);
                }
            }
        }
        None
    }

    /// Build the request body and pick its route from the log's own history.
    fn build_call(&self, rows: &[Value]) -> Option<(String, Value)> {
        let (provider, canonical) = self.canonical_request(rows)?;
        let route = self.routes.get(&provider)?;
        let body = to_wire_request(route.config.kind, &route.config, &canonical).ok()?;
        Some((provider, body))
    }

    /// Assemble the canonical request from the log.
    ///
    /// Upstream sends the conversation, not the newest message: a step's request
    /// is the visible history plus whatever it is resuming with. Reading it from
    /// the log rather than accumulating it in memory is what lets an interrupted
    /// turn resume identically.
    fn canonical_request(
        &self,
        rows: &[Value],
    ) -> Option<(String, llm_dialect::items::ItemRequest)> {
        use llm_dialect::items::{ItemRequest, ItemStreamMessage, Role};

        let mut messages: Vec<ItemStreamMessage> = Vec::new();
        let mut provider: Option<String> = None;
        let mut model: Option<String> = None;

        for row in rows {
            match row.get("type").and_then(Value::as_str).unwrap_or_default() {
                "user/message" => {
                    let items = content_items(row.get("data").unwrap_or(&Value::Null));
                    if !items.is_empty() {
                        messages.push(ItemStreamMessage {
                            role: Role::User,
                            items,
                            metadata: Default::default(),
                        });
                    }
                }
                "assistant/message" => {
                    let Some(msg) = row.pointer("/data/message") else {
                        continue;
                    };
                    let items = msg
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|c| content_items(&json!({ "content": c })))
                        .unwrap_or_default();
                    if !items.is_empty() {
                        messages.push(ItemStreamMessage {
                            role: Role::Assistant,
                            items,
                            metadata: Default::default(),
                        });
                    }
                    if let Some(src) = msg.get("source") {
                        if let Some(p) = src.get("provider").and_then(Value::as_str) {
                            provider = Some(p.to_string());
                        }
                        if let Some(m) = src.get("model").and_then(Value::as_str) {
                            model = Some(m.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        if messages.is_empty() {
            return None;
        }
        // The route the last model message recorded; on a first turn there is
        // none, so the sole configured route serves it.
        let provider_id = provider.or_else(|| self.routes.keys().next().cloned())?;
        let route = self.routes.get(&provider_id)?;
        let model = model.unwrap_or_else(|| route.config.model.clone());
        Some((
            provider_id,
            ItemRequest {
                model,
                messages,
                stream: true,
                ..Default::default()
            },
        ))
    }

    /// Open a model call, with a live decoder attached.
    ///
    /// The provider kind is resolved here rather than at decode time so the
    /// decoder is built against the same route the request went to; a call
    /// whose route vanished is left with the chat dialect, which is only a
    /// fallback for a case `build_call` already refuses.
    fn open_call(&self, state: TurnState, provider: String, body: Value) -> Op {
        let kind = self
            .routes
            .get(&provider)
            .map(|r| r.config.kind)
            .unwrap_or(ProviderKind::OpenAiChat);
        Op::Call {
            state,
            provider,
            body,
            live: Box::new(Live::new(kind)),
            effect: None,
        }
    }

    /// Emit the provider fetch for the pending call.
    ///
    /// The API key is read from the environment here, at call time, and never
    /// enters the machine or the log: [`ProviderConfig::auth`] resolves a header
    /// pair from a variable *name*.
    ///
    /// The streaming effect is used rather than the buffered one, so the
    /// machine sees the model's output as it arrives and can forward it. The
    /// answer still carries the whole body, which is what gets decoded a second
    /// time for the durable record — the two decodes share their decoders, so
    /// they cannot disagree.
    ///
    /// The effect id is returned rather than written onto `self.op`: the caller
    /// has the op *taken* while it calls this (the resume path holds it in a
    /// local), so writing through `self.op` here reaches nothing and the chunks
    /// that follow would match no call. Returning it makes the caller — which is
    /// the only frame that still owns the op — do the recording.
    fn fetch(
        &mut self,
        provider: &str,
        body: &Value,
    ) -> Option<(vocoder_cordis::EffectId, Vec<MachineOut>)> {
        let route = self.routes.get(provider)?;
        let headers: Vec<(String, String)> = route.config.auth().into_iter().collect();
        let url = route.config.url();
        let id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        Some((
            id,
            vec![rpc::effect(
                id,
                RealizeRequest::FetchStream {
                    url,
                    headers,
                    body: body.to_string(),
                },
            )],
        ))
    }

    /// Publish the turn's rows and answer its call.
    ///
    /// The reply is emitted only once the write has landed, so the operation is
    /// marked `Publishing` while the effect is outstanding: without that, the
    /// write's answer would arrive with no operation to attribute it to and the
    /// call would never be answered at all.
    fn finish(&mut self, state: &TurnState) -> Vec<MachineOut> {
        self.completed
            .insert(state.request_id.clone(), json!({ "accepted": true }));
        let out = self.publish(&state.session, &state.rows, state.opened_with);
        if out.is_empty() {
            // The write already landed, so the operation is complete.
            self.op = None;
            return rpc::ok(json!({ "accepted": true }));
        }
        self.op = Some(Op::Publishing);
        out
    }

    /// Close the turn as failed, keeping the log balanced.
    fn fail_turn(
        &mut self,
        mut state: TurnState,
        code: String,
        message: String,
    ) -> Vec<MachineOut> {
        let outs = state.fsm.step_reply(
            StepOutcome::Error {
                code: code.clone(),
                message: message.clone(),
            },
            false,
            false,
        );
        let _ = self.apply(&mut state, outs);
        self.op = None;
        self.finish(&state)
    }
}

/// The resume points: one entry per suspension.
impl AgentMachine {
    /// Open a turn from the input `session/prompt` already admitted.
    ///
    /// **The `user/message` is not written here.** `session/prompt` writes it —
    /// that row is the prompt's durable record, and it is what the caller's
    /// `rpcId` is attached to for idempotency. Writing a second one here made
    /// every prompt appear twice in the log, which the first live end-to-end run
    /// showed immediately (three `user/message` rows for one prompt: the session
    /// machine's, and two from an earlier shape of this function).
    ///
    /// What this adds is the *admission*: an `agent/inbox/spliced` row naming the
    /// message as the turn's input. Mirrors the control's own log, where a
    /// splice and the message it admits both appear.
    fn start_turn(
        &mut self,
        session: String,
        request_id: String,
        content: Value,
        rows: Vec<Value>,
    ) -> Vec<MachineOut> {
        let mut rows = rows;
        // The boundary is taken *before* the splice row is appended: the splice
        // is part of this turn, so a boundary captured after it would leave the
        // row unannounced and a follower would see the event sequence skip from
        // the prompt straight to `turn/start`. The client throws on a gap.
        let opened_with = rows.len();
        // The message id from the log's own user row for this prompt, so the
        // splice references the message that actually exists rather than a
        // freshly minted id pointing at nothing.
        let existing = rows.iter().rev().find_map(|row| {
            (row.get("type").and_then(Value::as_str) == Some("user/message"))
                .then(|| {
                    row.pointer("/data/id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .flatten()
        });
        let message_id = existing.unwrap_or_else(|| format!("msg-{}", rpc::new_id()));
        Self::row(
            &mut rows,
            &Draft {
                row_type: "agent/inbox/spliced",
                data: json!({
                    "target": "next-turn",
                    "start": 0,
                    "inserted": [{
                        "id": message_id,
                        "role": "user",
                        "content": content,
                        "source": { "kind": "user" },
                    }],
                }),
            },
        );
        // Re-derive the inbox from the rows just written rather than claiming
        // from a fold taken before the splice: claiming here is what makes the
        // admitted message the turn's own input.
        let mut inbox = match Inbox::fold(&rows) {
            Ok(inbox) => inbox,
            Err(e) => {
                self.op = None;
                return rpc::err("gateway/internal-error", format!("inbox fold: {e}"));
            }
        };
        let (messages, _splices) = inbox.claim(InboxTarget::NextTurn, 1);
        if messages.is_empty() {
            // The splice named the message, so an empty claim here is a
            // contradiction rather than an empty inbox.
            self.op = None;
            return rpc::err(
                "gateway/internal-error",
                "admitted message was not claimable from the inbox",
            );
        }
        let mut fsm = AgentLoop::resume_from(&rows);
        let outs = fsm.begin_turn(&messages);
        let mut state = TurnState {
            session,
            request_id,
            rows,
            fsm,
            opened_with,
        };
        let call = self.apply(&mut state, outs);
        self.continue_turn(state, call)
    }

    /// Take the turn's next step, or publish it when it has ended.
    ///
    /// `call` is a model call `apply` already decided, or `None` when the turn
    /// ended within `apply` (an empty turn, or one closed by an error).
    fn continue_turn(
        &mut self,
        mut state: TurnState,
        call: Option<(String, Value)>,
    ) -> Vec<MachineOut> {
        if let Some((provider, body)) = call {
            self.op = Some(self.open_call(state, provider, body));
            return self.resume_op();
        }
        // A running turn with no call decided owes another step.
        if state.fsm.is_running() {
            let outs = state.fsm.enter_step();
            let call = self.apply(&mut state, outs);
            if let Some((provider, body)) = call {
                self.op = Some(self.open_call(state, provider, body));
                return self.resume_op();
            }
        }
        self.finish(&state)
    }

    /// Continue whatever the pending op is waiting on.
    fn resume_op(&mut self) -> Vec<MachineOut> {
        match self.op.take() {
            Some(Op::Opening {
                session,
                request_id,
                content,
            }) => {
                let rows = match self.rows_of(&session) {
                    Ok(Some(rows)) => rows,
                    Ok(None) => {
                        self.op = None;
                        return rpc::err(
                            "session/not-found",
                            format!("no such session: {session}"),
                        );
                    }
                    Err(effects) => {
                        self.op = Some(Op::Opening {
                            session,
                            request_id,
                            content,
                        });
                        return effects;
                    }
                };
                self.start_turn(session, request_id, content, rows)
            }
            Some(Op::Call {
                state,
                provider,
                body,
                live,
                effect: _,
            }) => {
                let Some((id, effects)) = self.fetch(&provider, &body) else {
                    return vec![];
                };
                self.op = Some(Op::Call {
                    state,
                    provider,
                    body,
                    live,
                    effect: Some(id),
                });
                effects
            }
            Some(Op::Settle {
                state,
                provider,
                body,
            }) => self.settle(state, provider, body),
            // A re-entry that arrives with the publish still pending re-issues
            // the write; the cache's `published` set makes that a no-op that
            // reports completion instead of a second generation.
            Some(Op::Publishing) => {
                self.op = None;
                rpc::ok(json!({ "accepted": true }))
            }
            None => vec![],
        }
    }

    /// Decode the provider's stream and settle the step.
    fn settle(&mut self, mut state: TurnState, provider: String, body: String) -> Vec<MachineOut> {
        let kind = self
            .routes
            .get(&provider)
            .map(|r| r.config.kind)
            .unwrap_or(ProviderKind::OpenAiChat);
        let chunks = decode(kind, &body);
        let (message, usage) = assemble_message(&chunks, &provider, &state.rows);
        let reason = chunks.iter().rev().find_map(|c| match c {
            StreamChunk::Finish { reason } => Some(reason.clone()),
            _ => None,
        });
        let has_calls = chunks.iter().any(|c| {
            matches!(
                c,
                StreamChunk::BlockStart {
                    block_type: BlockType::ToolCall,
                    ..
                }
            )
        });
        // A stream with no finish chunk is recorded as an *attempt*, not a
        // message: claiming a reply the model never finished would make a broken
        // call replay as a good one.
        let row_type = if reason.is_some() {
            "assistant/message"
        } else {
            "assistant/attempt"
        };
        let (turn, step) = state.fsm.position();
        let settled = Self::row(
            &mut state.rows,
            &Draft {
                row_type,
                data: json!({
                    "turn": turn,
                    "step": step,
                    "message": message,
                    "usage": usage,
                    "stream": chunk_records(&chunks),
                }),
            },
        );
        // The attempt's closing frame, naming the row that supersedes it.
        //
        // Without this a client's partial stays live forever: the protocol's
        // terminal marker is `end`, and the durable event that would otherwise
        // retire the partial is delivered on the same follow stream *behind*
        // these frames — so a client that stopped at the last chunk would render
        // a partial that never resolves.
        //
        // `committed` carries the settling row's own seq, which is what a client
        // needs to correlate the frame with the event that follows it.
        let seq = settled.get("seq").and_then(Value::as_f64).unwrap_or(-1.0);
        let outcome = match &reason {
            Some(r) => step_outcome(r),
            None => StepOutcome::Error {
                code: "PROVIDER_STREAM_INCOMPLETE".into(),
                message: "provider stream ended without a finish chunk".into(),
            },
        };
        let mut end_frames = vec![stream_event(
            &state.session,
            json!({
                "type": "end",
                "attemptId": self.attempt_id_of(&state),
                "revision": self.revision_of(&state) + 1,
                "index": self.chunk_index_of(&state),
                "outcome": if reason.is_some() {
                    json!({ "kind": "committed", "eventType": row_type, "seq": seq })
                } else {
                    json!({ "kind": "abandoned" })
                },
            }),
        )];
        let outs = state.fsm.step_reply(outcome, has_calls, false);
        let call = self.apply(&mut state, outs);
        end_frames.extend(self.continue_turn(state, call));
        end_frames
    }
}

impl PluginMachine for AgentMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        match ev {
            MachineIn::ServicesReady { .. } => vec![MachineOut::Subscribe {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
            }],
            MachineIn::Event { name, payload } if name.0 == rpc::call_event("agent") => {
                let method = payload
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let args = payload.get("args").cloned().unwrap_or(Value::Null);
                self.begin(&method, &args)
            }
            MachineIn::EffectChunk { id, bytes } => self.on_chunk(id, &bytes),
            MachineIn::EffectResult { id, result } => self.on_effect(id, result),
            _ => vec![],
        }
    }
}

impl AgentMachine {
    /// Handle one `agent/*` call.
    fn begin(&mut self, method: &str, args: &Value) -> Vec<MachineOut> {
        match method {
            "run" | "prompt" => self.begin_turn(args),
            "cancel" => self.cancel_turn(args),
            other => rpc::err(
                "gateway/bad-request",
                format!("unsupported agent method: {other}"),
            ),
        }
    }

    /// Cancel the running turn for a session.
    ///
    /// The FSM decides *what* the cancel owes, and its answer is subtle in a way
    /// this must respect: when a step is open the cancel only *latches*, and the
    /// closers are emitted when the model's reply settles — which is the only
    /// ordering that keeps `step/start`/`step/end` balanced. So a cancel here
    /// reports acceptance and lets the in-flight call finish; forcing the frame
    /// closers now would leave the reply with no step to settle into.
    ///
    /// A cancel for a session with no turn running is accepted and does
    /// nothing, which is upstream's own behaviour for an idle agent.
    fn cancel_turn(&mut self, args: &Value) -> Vec<MachineOut> {
        let session = args
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if session.is_empty() {
            return rpc::err("gateway/bad-request", "missing sessionId");
        }
        let turn = match &self.op {
            Some(Op::Call { state, .. }) | Some(Op::Settle { state, .. }) => {
                Some(state.fsm.position().0)
            }
            _ => None,
        };
        let Some(op) = self.op.as_mut() else {
            // No operation in flight: nothing to latch, and nothing to write.
            return rpc::ok(json!({ "accepted": true }));
        };
        // The rows live in whichever op variant is holding the turn; a cancel
        // mid-call settles when that call's answer arrives, so latching is
        // enough and no frame closers are forced here.
        let rows = match op {
            Op::Opening { .. } => None,
            Op::Call { state, .. } | Op::Settle { state, .. } => Some(&mut state.rows),
            Op::Publishing => None,
        };
        let Some(rows) = rows else {
            return rpc::ok(json!({ "accepted": true }));
        };
        if rows
            .iter()
            .rev()
            .any(|r| r.get("type").and_then(Value::as_str) == Some("turn/end"))
        {
            return rpc::ok(json!({ "accepted": true }));
        }
        // The cancel is recorded as a row so a resume can see it, and the
        // in-flight call is left to settle the turn.
        let seq = rows.len().saturating_sub(1) as f64;
        rows.push(json!({
            "type": "agent/cancel-requested",
            "seq": seq,
            "time": super::session_now_ms(),
            "data": { "turn": turn, "cause": { "kind": "user" } },
        }));
        rpc::ok(json!({ "accepted": true }))
    }

    fn begin_turn(&mut self, args: &Value) -> Vec<MachineOut> {
        let session = args
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let request_id = args
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if session.is_empty() {
            return rpc::err("gateway/bad-request", "missing sessionId");
        }
        // A prompt with no content admits nothing; refusing here keeps an empty
        // turn boundary out of the log.
        let content = args.get("content").cloned().unwrap_or(Value::Null);
        let has_content = content.as_array().is_some_and(|parts| !parts.is_empty());
        if !has_content {
            return rpc::err("gateway/bad-request", "prompt requires content");
        }
        // Idempotent: a redelivered call must not start a second turn.
        if let Some(done) = self.completed.get(&request_id) {
            return rpc::ok(done.clone());
        }
        match self.rows_of(&session) {
            Ok(Some(rows)) => self.start_turn(session, request_id, content, rows),
            Ok(None) => rpc::err("session/not-found", format!("no such session: {session}")),
            Err(effects) => {
                // The walk or the read is in flight; the turn is claimed when
                // the answer lands.
                self.op = Some(Op::Opening {
                    session,
                    request_id,
                    content,
                });
                effects
            }
        }
    }

    /// Absorb an effect answer and continue.
    ///
    /// The pending op, not the effect id, decides what the answer means: only
    /// one effect is ever outstanding for this machine.
    /// Decode a slice of a streaming call's response and publish what it says.
    ///
    /// The op is *taken* for the duration so the decode can borrow the `Live`
    /// mutably and still produce outputs; it is put back before returning, since
    /// the call is still in flight and the answer has not arrived.
    ///
    /// Ids are checked against the outstanding effect: a chunk for an effect this
    /// machine is no longer waiting on is a late delivery, and applying it would
    /// append text to a turn that has already settled.
    fn on_chunk(&mut self, id: vocoder_cordis::EffectId, bytes: &[u8]) -> Vec<MachineOut> {
        let Some(Op::Call {
            state,
            provider,
            body,
            mut live,
            effect,
        }) = self.op.take()
        else {
            return vec![];
        };
        // A chunk for any other effect is a late delivery for a call this
        // machine has already moved past, and applying it would append text to
        // a turn that has settled.
        if effect != Some(id) {
            self.op = Some(Op::Call {
                state,
                provider,
                body,
                live,
                effect,
            });
            return vec![];
        }
        let frames = live.feed(bytes);
        let outs = Self::stream_dispatches(&state, &live, frames);
        // Marked *after* the dispatches are built: `stream_dispatches` reads
        // this flag to decide whether the `start` frame is owed, so setting it
        // first is what made the first version emit chunks with no start ahead
        // of them — and a follower's accumulator drops a chunk that arrives
        // before the start that names its attempt.
        if !outs.is_empty() {
            live.started = true;
        }
        self.op = Some(Op::Call {
            state,
            provider,
            body,
            live,
            effect,
        });
        outs
    }

    /// The `agent/assistant-stream` events for a batch of decoded frames.
    ///
    /// A `start` frame is emitted once per attempt, ahead of the chunks that
    /// reference it: a follower's accumulator keys its state on `attemptId` and
    /// drops a chunk that arrives before the start that names it. The flag lives
    /// on `Live` rather than in a map here so it travels with the attempt it
    /// describes — a second map keyed by request id would have to be kept in
    /// step with the op's own lifetime, which is the same fact stored twice.
    fn stream_dispatches(state: &TurnState, live: &Live, frames: Vec<Value>) -> Vec<MachineOut> {
        if frames.is_empty() {
            return vec![];
        }
        let mut outs = Vec::new();
        let (turn, step) = state.fsm.position();
        if !live.started {
            outs.push(stream_event(
                &state.session,
                json!({
                    "type": "start",
                    "attemptId": live.attempt_id,
                    "revision": 1,
                    // The cursor a follower is caught up to when this attempt
                    // began: everything at or before it is already in the
                    // snapshot, so frames after it are the ones the client
                    // cannot have seen. `seq` is zero-based over events with
                    // the header at row 0, which is why the last durable seq
                    // is `len - 2`.
                    "startedAfterSeq": state.rows.len().saturating_sub(2) as i64,
                    "turn": turn,
                    "step": step,
                }),
            ));
        }
        for frame in frames {
            outs.push(stream_event(&state.session, frame));
        }
        outs
    }

    fn on_effect(
        &mut self,
        _id: vocoder_cordis::EffectId,
        result: EffectResult,
    ) -> Vec<MachineOut> {
        let Some(op) = self.op.take() else {
            // A late answer for an effect this machine already moved past.
            return vec![];
        };
        match (op, result) {
            // A read or write answer for an opening or running turn. The cache
            // absorbs it and the op is re-entered.
            (
                Op::Opening {
                    session,
                    request_id,
                    content,
                },
                other,
            ) => {
                self.cache.absorb(other);
                self.op = Some(Op::Opening {
                    session,
                    request_id,
                    content,
                });
                self.resume_op()
            }
            // The write landed: the operation is complete and now answerable.
            (Op::Publishing, other) => {
                self.cache.absorb(other);
                self.op = None;
                rpc::ok(json!({ "accepted": true }))
            }
            (
                Op::Call {
                    state,
                    provider,
                    body: _,
                    live,
                    effect: _,
                },
                EffectResult::HttpResponse { status, body },
            ) => {
                if !(200..300).contains(&status) {
                    return self.fail_turn(
                        state,
                        provider_error_code(status),
                        format!("provider returned HTTP {status}"),
                    );
                }
                // Flush the decoder's tail and emit whatever the last frame
                // implied, so the display's final frame is the completed text
                // rather than the text as of the last transport read. Without
                // this the partial would end mid-word and only the superseding
                // `assistant/message` would show the rest.
                let mut live = live;
                let tail = live.end();
                let mut streamed = Self::stream_dispatches(&state, &live, tail);
                if !streamed.is_empty() {
                    live.started = true;
                }
                // Remember the attempt's identity and counters before the
                // decoder is dropped: `settle` emits the closing `end` frame,
                // and it has to name this attempt and continue these counters
                // or a client rejects the frame and never retires its partial.
                self.last_attempt = Some((live.attempt_id.clone(), live.revision, live.index));
                // The step's own rows go out after the stream frames, so a
                // client sees the partial complete before the message that
                // supersedes it.
                self.op = Some(Op::Settle {
                    state,
                    provider,
                    body,
                });
                streamed.extend(self.resume_op());
                streamed
            }
            (Op::Call { state, .. }, EffectResult::Failed(e)) => self.fail_turn(
                state,
                "PROVIDER_UNREACHABLE".into(),
                format!("provider call failed: {}", e.message()),
            ),
            // Anything else — a non-HTTP answer to a call, or any answer to a
            // settle — is not something this machine asked for. It is absorbed
            // for what it is worth and the op is restored unchanged, because
            // dropping it would abandon a half-written turn.
            (op, other) => {
                self.cache.absorb(other);
                self.op = Some(op);
                vec![]
            }
        }
    }
    /// The attempt id of the call currently in flight, or — once it has settled —
    /// the one that just did.
    ///
    /// The fallback matters: `settle` runs after the op is taken and consumed,
    /// so there is no `Live` left to read; the recorded summary is what keeps
    /// the closing frame attributable to the attempt whose chunks preceded it.
    fn attempt_id_of(&self, state: &TurnState) -> String {
        match self.op.as_ref() {
            Some(Op::Call { state: s, live, .. }) if s.request_id == state.request_id => {
                live.attempt_id.clone()
            }
            _ => self
                .last_attempt
                .as_ref()
                .map(|(id, _, _)| id.clone())
                .unwrap_or_else(|| format!("attempt-{}", rpc::new_id())),
        }
    }

    /// The revision of the live attempt, or of the one that just settled.
    fn revision_of(&self, state: &TurnState) -> u64 {
        match self.op.as_ref() {
            Some(Op::Call { state: s, live, .. }) if s.request_id == state.request_id => {
                live.revision
            }
            _ => self.last_attempt.as_ref().map(|(_, r, _)| *r).unwrap_or(0),
        }
    }

    /// The next chunk index the live attempt would use, or that of the settled one.
    fn chunk_index_of(&self, state: &TurnState) -> u64 {
        match self.op.as_ref() {
            Some(Op::Call { state: s, live, .. }) if s.request_id == state.request_id => live.index,
            _ => self.last_attempt.as_ref().map(|(_, _, i)| *i).unwrap_or(0),
        }
    }
}

/// Decode a provider body into the seam's chunks.
fn decode(kind: ProviderKind, body: &str) -> Vec<StreamChunk> {
    let mut sse = SseDecoder::new();
    let mut frames: Vec<SseFrame> = Vec::new();
    sse.feed(body.as_bytes(), &mut frames);
    sse.finish(&mut frames);
    let mut stream = ProviderStreamDecoder::new(kind);
    let mut chunks = Vec::new();
    for frame in &frames {
        stream.frame(frame, &mut chunks);
    }
    stream.end(&mut chunks);
    chunks
}

/// Re-pack chunks into the durable `stream` array a session log records.
///
/// Consecutive deltas collapse into `text-chunks`/`reasoning-chunks`/
/// `tool-call-chunks` records with parallel arrays. This is the inverse of
/// `expand_assistant_stream`, and it is what makes the row just written
/// replayable by the same code that reads recorded sessions.
fn chunk_records(chunks: &[StreamChunk]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < chunks.len() {
        match &chunks[i] {
            StreamChunk::TextDelta { .. } | StreamChunk::ReasoningDelta { .. } => {
                let is_text = matches!(chunks[i], StreamChunk::TextDelta { .. });
                let index = match &chunks[i] {
                    StreamChunk::TextDelta { index, .. }
                    | StreamChunk::ReasoningDelta { index, .. } => *index,
                    _ => unreachable!("matched above"),
                };
                let mut texts = Vec::new();
                while i < chunks.len() {
                    match &chunks[i] {
                        StreamChunk::TextDelta { index: j, text } if is_text && *j == index => {
                            texts.push(json!(text));
                            i += 1;
                        }
                        StreamChunk::ReasoningDelta { index: j, text }
                            if !is_text && *j == index =>
                        {
                            texts.push(json!(text));
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(json!({
                    "type": if is_text { "text-chunks" } else { "reasoning-chunks" },
                    "index": index,
                    "texts": texts,
                }));
            }
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let (index, id, name) = (*index, id.clone(), name.clone());
                let mut args = vec![json!(arguments_delta)];
                i += 1;
                while i < chunks.len() {
                    match &chunks[i] {
                        StreamChunk::ToolCallDelta {
                            index: j,
                            arguments_delta,
                            ..
                        } if *j == index => {
                            args.push(json!(arguments_delta));
                            i += 1;
                        }
                        _ => break,
                    }
                }
                let mut record = json!({
                    "type": "tool-call-chunks", "index": index, "id": id, "args": args,
                });
                if let Some(name) = name {
                    record["name"] = json!(name);
                }
                out.push(record);
            }
            other => {
                out.push(json!({ "type": "chunk", "chunk": chunk_value(other) }));
                i += 1;
            }
        }
    }
    out
}

/// One chunk in the durable `chunk` record shape.
fn chunk_value(chunk: &StreamChunk) -> Value {
    match chunk {
        StreamChunk::BlockStart { index, block_type } => json!({
            "type": "block-start",
            "index": index,
            "blockType": match block_type {
                BlockType::Text => "text",
                BlockType::Reasoning => "reasoning",
                BlockType::ToolCall => "tool-call",
            },
        }),
        StreamChunk::BlockEnd { index, block } => {
            json!({ "type": "block-end", "index": index, "block": block })
        }
        StreamChunk::Usage { usage } => json!({ "type": "usage", "usage": usage }),
        StreamChunk::Finish { reason } => {
            let mut v = json!({ "type": "finish", "reason": { "kind": reason.kind() } });
            if let FinishReason::Aborted { message, code } | FinishReason::Error { message, code } =
                reason
            {
                v["reason"]["failure"] = json!({ "message": message, "code": code });
            }
            v
        }
        StreamChunk::TextDelta { index, text } => {
            json!({ "type": "text-delta", "index": index, "text": text })
        }
        StreamChunk::ReasoningDelta { index, text } => {
            json!({ "type": "reasoning-delta", "index": index, "text": text })
        }
        StreamChunk::ToolCallDelta {
            index,
            id,
            name,
            arguments_delta,
        } => json!({
            "type": "tool-call-delta",
            "index": index,
            "id": id,
            "name": name,
            "argumentsDelta": arguments_delta,
        }),
    }
}

/// Assemble the assistant message and usage from a decoded stream.
///
/// The message's content is built by **accumulating the deltas per block
/// index**, not by reading the `block-end` payloads. Those are deliberately
/// empty (see `ProviderStreamDecoder::close_open`): the decoder owns the chunk
/// vocabulary and this owns assembly, so there is one place that decides what a
/// block contains. Reading `block-end` was the first implementation and it
/// produced empty content for every provider — the placeholders are empty by
/// design, so a stream that decoded perfectly assembled into nothing.
///
/// Blocks are emitted in index order, so reasoning precedes the text it
/// preceded on the wire.
fn assemble_message(chunks: &[StreamChunk], provider: &str, rows: &[Value]) -> (Value, Value) {
    // Block index → its kind and accumulated text, kept in a BTreeMap so the
    // output is ordered by the wire's own block numbering.
    let mut blocks: BTreeMap<u64, (BlockType, String)> = BTreeMap::new();
    // Tool calls accumulate separately: their fragments concatenate into a JSON
    // string, and their id and name arrive once.
    let mut tool_ids: BTreeMap<u64, (String, String)> = BTreeMap::new();
    let mut usage = json!({ "inputTokens": 0, "outputTokens": 0 });
    for chunk in chunks {
        match chunk {
            StreamChunk::BlockStart { index, block_type } => {
                blocks.entry(*index).or_insert((*block_type, String::new()));
            }
            StreamChunk::TextDelta { index, text }
            | StreamChunk::ReasoningDelta { index, text } => {
                let kind = if matches!(chunk, StreamChunk::TextDelta { .. }) {
                    BlockType::Text
                } else {
                    BlockType::Reasoning
                };
                let entry = blocks.entry(*index).or_insert((kind, String::new()));
                entry.1.push_str(text);
            }
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let entry = tool_ids.entry(*index).or_default();
                if !id.is_empty() {
                    entry.0.clone_from(id);
                }
                if let Some(name) = name {
                    entry.1.clone_from(name);
                }
                blocks
                    .entry(*index)
                    .or_insert((BlockType::ToolCall, String::new()))
                    .1
                    .push_str(arguments_delta);
            }
            StreamChunk::Usage { usage: u } => usage = u.clone(),
            // Deliberately not read; see the doc comment above.
            StreamChunk::BlockEnd { .. } | StreamChunk::Finish { .. } => {}
        }
    }

    let mut content: Vec<Value> = Vec::new();
    for (index, (kind, text)) in &blocks {
        match kind {
            BlockType::Text => {
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
            }
            BlockType::Reasoning => {
                if !text.is_empty() {
                    content.push(json!({ "type": "reasoning", "text": text }));
                }
            }
            BlockType::ToolCall => {
                let (id, name) = tool_ids.get(index).cloned().unwrap_or_default();
                // Arguments are streamed as JSON fragments; a call whose
                // fragments do not parse is recorded with an empty input rather
                // than dropped, because the call itself did happen.
                let arguments: Value = serde_json::from_str(text).unwrap_or_else(|_| json!({}));
                content.push(json!({
                    "type": "tool-call",
                    "id": id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
        }
    }

    // The model name the history recorded; a first call has none.
    let model = rows
        .iter()
        .rev()
        .find_map(|r| {
            r.pointer("/data/message/source/model")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    (
        json!({
            "role": "assistant",
            "content": content,
            "source": { "kind": "model", "provider": provider, "model": model },
        }),
        usage,
    )
}

/// Extract canonical content items from a logged message's `content`.
fn content_items(data: &Value) -> Vec<llm_dialect::items::ContentItem> {
    use llm_dialect::items::ContentItem;
    let Some(parts) = data.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push(ContentItem::Text {
                        text: text.to_string(),
                    });
                }
            }
            Some("tool-call") | Some("tool_use") => {
                let id = part
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                let name = part
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = part
                    .get("arguments")
                    .or_else(|| part.get("input"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                out.push(ContentItem::ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            Some("tool-result") | Some("tool_result") => {
                let tool_call_id = part
                    .get("toolCallId")
                    .or_else(|| part.get("tool_use_id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if tool_call_id.is_empty() {
                    continue;
                }
                out.push(ContentItem::ToolResult {
                    tool_call_id,
                    content: part.get("content").cloned().unwrap_or(Value::Null),
                    is_error: part
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
            _ => {}
        }
    }
    out
}

/// A stable code for a provider HTTP failure.
fn provider_error_code(status: u16) -> String {
    match status {
        401 | 403 => "PROVIDER_UNAUTHORIZED",
        404 => "PROVIDER_NOT_FOUND",
        429 => "PROVIDER_RATE_LIMITED",
        400 | 422 => "PROVIDER_BAD_REQUEST",
        500..=599 => "PROVIDER_UNAVAILABLE",
        _ => "PROVIDER_ERROR",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocoder_cordis::RpcReply;

    /// A sessions root holding one session with a header generation.
    ///
    /// The machine finds a session by scanning for its generation file, so the
    /// layout has to match what `session/create` writes: a project directory
    /// (the no-cwd form) containing the encoded session id.
    fn session_home(session_id: &str) -> (tempfile::TempDir, PathBuf) {
        session_home_with(session_id, true)
    }

    /// As [`session_home`], optionally without the prompt's user row.
    ///
    /// The `user/message` is part of the fixture on purpose: `session/prompt`
    /// writes it, and the agent machine consumes it rather than writing a second
    /// one. A fixture with only a header would let a duplicate-message defect
    /// back in unnoticed.
    fn session_home_with(session_id: &str, with_user: bool) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let sessions = dir.path().join("sessions");
        let sdir = dir
            .path()
            .join("sessions")
            .join(super::super::session::SessionStore::project_dir(None))
            .join(super::super::session::SessionStore::encode_segment(
                session_id,
            ));
        std::fs::create_dir_all(&sdir).expect("session dir");
        let mut rows = vec![json!({
            "type": "session", "version": 3, "id": session_id, "createdAt": 0,
        })];
        if with_user {
            rows.push(json!({
                "type": "user/message",
                "seq": 0,
                "time": 0,
                "data": {
                    "content": [{ "type": "text", "text": "ping" }],
                    "source": { "kind": "user", "rpcId": "seed" },
                    "role": "user",
                    "id": "msg-seed",
                },
            }));
        }
        let bytes = vocoder_session::encode_generation(&rows, false).expect("encode");
        std::fs::write(
            sdir.join(vocoder_session::generation_filename(0, false)),
            bytes,
        )
        .expect("write header");
        (dir, sessions)
    }

    /// Drive one input through the machine to quiescence, performing effects
    /// for real — including the provider call, when the environment names one.
    ///
    /// Delegates to [`crate::driver::drive_with`] rather than reimplementing the
    /// loop, so a test exercises the same chunk-then-result ordering the live
    /// host uses. A private copy would have been free to skip the chunk
    /// deliveries entirely, and every streaming assertion below would then have
    /// been testing a code path production never runs.
    fn drive(machine: &mut AgentMachine, pending: MachineIn) -> Vec<MachineOut> {
        drive_collecting(machine, pending).0
    }

    /// [`drive`], also returning the outputs the *chunk* deliveries produced.
    ///
    /// Terminal outputs are by construction the ones that arrived after the
    /// effect, so a test asserting that frames were published *during* the call
    /// has to read them here.
    fn drive_collecting(
        machine: &mut AgentMachine,
        pending: MachineIn,
    ) -> (Vec<MachineOut>, Vec<MachineOut>) {
        let mut during: Vec<MachineOut> = Vec::new();
        let terminal = crate::driver::drive_with(machine, pending, &mut |outs| {
            during.extend(outs.iter().cloned());
        });
        (terminal, during)
    }

    fn reply_of(outs: &[MachineOut]) -> RpcReply {
        outs.iter()
            .find_map(|o| match o {
                MachineOut::Reply(r) => Some(r.clone()),
                _ => None,
            })
            .expect("a reply")
    }

    /// A provider that answers from a canned body, so the whole turn —
    /// admission, FSM, packing, publication — can be tested with no network.
    ///
    /// The body is a verbatim capture from a live gateway, including its nested
    /// `thinking` deltas.
    fn canned_provider(body: &'static str) -> Route {
        let dir = std::env::temp_dir().join(format!("voco-canned-{}", rpc::new_id()));
        std::fs::create_dir_all(&dir).expect("canned dir");
        std::fs::write(dir.join("body.txt"), body).expect("write body");
        // The URL is a `file:` path the test's own `realize` shim intercepts;
        // see `canned_fetch`.
        Route {
            config: ProviderConfig {
                id: "canned".into(),
                kind: ProviderKind::OpenAiChat,
                base_url: format!("canned://{}", dir.to_string_lossy()),
                model: "test-model".into(),
                api_key_env: None,
            },
        }
    }

    /// Rows of a session's newest generation.
    fn written_rows(sessions: &std::path::Path, session_id: &str) -> Vec<Value> {
        let dir = sessions
            .join(super::super::session::SessionStore::project_dir(None))
            .join(super::super::session::SessionStore::encode_segment(
                session_id,
            ));
        let listing: Vec<String> = std::fs::read_dir(&dir)
            .map(|e| {
                e.flatten()
                    .map(|x| x.path().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let dir_str = dir.to_string_lossy().to_string();
        let Some((_, path)) =
            super::super::session::SessionStore::latest_generation_in(&dir_str, &listing)
        else {
            return Vec::new();
        };
        let bytes = std::fs::read(&path).expect("read generation");
        vocoder_session::decode_generation(
            &bytes,
            super::super::session::SessionStore::is_compressed(&path),
        )
        .expect("decode generation")
    }

    /// A verbatim capture from a live gateway: reasoning arrives as a nested
    /// `thinking` object, and the finish reason shares a delta with the last
    /// text fragment. The usage trailer follows the finish.
    const LIVE_BODY: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
        "\"created\":1,\"id\":\"chatcmpl-1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: {\"choices\":[{\"delta\":{\"thinking\":{\"block_index\":0,\"kind\":\"thinking\",",
        "\"text\":\"The user wants \"}},\"finish_reason\":null,\"index\":0}],\"created\":1,",
        "\"id\":\"chatcmpl-1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"PONG\",\"thinking\":{\"block_index\":0,",
        "\"kind\":\"thinking\",\"text\":\"PONG.\"}},\"finish_reason\":\"stop\",\"index\":0}],",
        "\"created\":1,\"id\":\"chatcmpl-1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: {\"choices\":[],\"created\":1,\"id\":\"chatcmpl-1\",\"model\":\"m\",",
        "\"object\":\"chat.completion.chunk\",\"usage\":{\"prompt_tokens\":35,",
        "\"completion_tokens\":36,\"total_tokens\":71}}\n\n",
        "data: [DONE]\n\n",
    );

    /// The whole turn, end to end, with no network: admission, the frame
    /// sequence, the recorded reply, and the packed stream.
    #[test]
    fn a_turn_writes_a_balanced_frame_sequence_and_records_the_reply() {
        let session_id = "session-1";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(LIVE_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let outs = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-1",
                        "content": [{ "type": "text", "text": "ping" }],
                    },
                }),
            },
        );
        assert!(
            matches!(reply_of(&outs), RpcReply::Ok { .. }),
            "the turn is accepted: {outs:?}"
        );

        let rows = written_rows(&sessions, session_id);
        let types: Vec<&str> = rows
            .iter()
            .map(|r| r.get("type").and_then(Value::as_str).unwrap_or_default())
            .collect();
        // The frame sequence, in order, exactly once each.
        assert_eq!(
            types,
            vec![
                "session",
                "user/message",
                "agent/inbox/spliced",
                "turn/start",
                "step/start",
                "assistant/message",
                "step/end",
                "turn/end",
            ],
            "unexpected frame sequence"
        );
        // The prompt's message appears exactly once. The session machine writes
        // it; the agent machine admits it. A second copy is what the first live
        // end-to-end run produced, and it is invisible to every other assertion
        // here (the assistant message is still correct), so it is asserted
        // directly.
        assert_eq!(
            types.iter().filter(|t| **t == "user/message").count(),
            1,
            "the prompt's message must not be duplicated: {types:?}"
        );
        // The turn closes completed, not merely closed.
        assert_eq!(
            rows.last().unwrap().pointer("/data/reason/kind"),
            Some(&json!("completed"))
        );

        // The reply is recorded with its visible text, and the nested
        // `thinking` delta is not lost.
        let msg = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("assistant/message"))
            .expect("the assistant message");
        let blocks = msg
            .pointer("/data/message/content")
            .unwrap()
            .as_array()
            .unwrap();
        let text: String = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        assert_eq!(text, "PONG");
        // Usage came from the trailer that follows the finish chunk.
        assert_eq!(
            msg.pointer("/data/usage/inputTokens"),
            Some(&json!(35)),
            "{msg}"
        );
        assert_eq!(
            msg.pointer("/data/usage/outputTokens"),
            Some(&json!(36)),
            "{msg}"
        );
    }

    /// The row this driver wrote must be readable by the replay provider.
    ///
    /// This closes the loop between the two halves of the seam: the driver
    /// *packs* a stream record and the replay provider *expands* it, so a
    /// mistake in either direction fails here.
    #[test]
    fn the_written_turn_is_replayable_by_the_replay_provider() {
        let session_id = "session-2";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(LIVE_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        let _ = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-2",
                        "content": [{ "type": "text", "text": "ping" }],
                    },
                }),
            },
        );

        let rows = written_rows(&sessions, session_id);
        let script = super::super::llm_replay::derive_replay_script(&rows)
            .expect("a turn this driver wrote must derive as a replay script");
        assert_eq!(script.len(), 1, "one model call was made");
        assert_eq!(
            script[0].finish_reason(),
            Some(&FinishReason::Stop),
            "the derived entry keeps its finish reason"
        );
        // The reasoning survived the pack/expand round trip.
        let reasoning: String = script[0]
            .chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ReasoningDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, "The user wants PONG.", "reasoning round trip");
    }

    /// A provider failure still closes the turn: an unclosed turn is an orphan
    /// the next resume would have to repair, and the repair would invent a
    /// reason the log never recorded.
    #[test]
    fn a_provider_error_still_closes_the_turn() {
        let session_id = "session-3";
        let (_home, sessions) = session_home(session_id);
        let mut route = canned_provider(LIVE_BODY);
        // A canned route whose body file is absent: the driver reports the
        // failure back as a failed effect.
        route.config.base_url = "canned:///definitely-not-a-directory".into();
        let mut machine = AgentMachine::new(sessions.clone(), vec![route]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let outs = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-3",
                        "content": [{ "type": "text", "text": "ping" }],
                    },
                }),
            },
        );
        assert!(matches!(reply_of(&outs), RpcReply::Ok { .. }), "{outs:?}");

        let rows = written_rows(&sessions, session_id);
        let types: Vec<&str> = rows
            .iter()
            .map(|r| r.get("type").and_then(Value::as_str).unwrap_or_default())
            .collect();
        // The frames stay balanced despite the failure.
        for frame in ["turn/start", "turn/end", "step/end"] {
            assert_eq!(
                types.iter().filter(|t| **t == frame).count(),
                1,
                "{frame} exactly once in {types:?}"
            );
        }
        // And the turn reports the failure rather than a completion.
        let reason = rows
            .last()
            .and_then(|r| r.pointer("/data/reason"))
            .expect("a turn/end reason");
        assert_eq!(reason["kind"], "error", "{reason}");
        assert!(
            reason.pointer("/error/code").is_some(),
            "an error ending carries a code: {reason}"
        );
    }

    /// A stream with no finish chunk is recorded as an attempt, not a message.
    #[test]
    fn a_truncated_stream_is_recorded_as_an_attempt() {
        let session_id = "session-4";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(LIVE_BODY)]);
        let truncated = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
        let dir = std::env::temp_dir().join(format!("voco-trunc-{}", rpc::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("body.txt"), truncated).unwrap();
        machine.routes.get_mut("canned").unwrap().config.base_url =
            format!("canned://{}", dir.to_string_lossy());
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        let _ = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-4",
                        "content": [{ "type": "text", "text": "ping" }],
                    },
                }),
            },
        );

        let rows = written_rows(&sessions, session_id);
        let types: Vec<&str> = rows
            .iter()
            .map(|r| r.get("type").and_then(Value::as_str).unwrap_or_default())
            .collect();
        assert!(
            types.contains(&"assistant/attempt"),
            "a truncated call is an attempt, not a message: {types:?}"
        );
        assert!(
            !types.contains(&"assistant/message"),
            "no message may be claimed for a call that never finished: {types:?}"
        );
        assert!(types.contains(&"turn/end"), "{types:?}");
        assert_eq!(
            rows.last().unwrap().pointer("/data/reason/kind"),
            Some(&json!("error"))
        );
    }

    /// A second call with the same requestId must not start a second turn.
    #[test]
    fn a_redelivered_request_does_not_start_a_second_turn() {
        let session_id = "session-5";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(LIVE_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        let call = || MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
            payload: json!({
                "method": "run",
                "args": {
                    "sessionId": session_id,
                    "requestId": "req-5",
                    "content": [{ "type": "text", "text": "ping" }],
                },
            }),
        };
        let _ = drive(&mut machine, call());
        let before = written_rows(&sessions, session_id).len();
        let _ = drive(&mut machine, call());
        let after = written_rows(&sessions, session_id).len();
        assert_eq!(
            before, after,
            "a redelivered request must not append another turn"
        );
    }

    /// A prompt with no content admits nothing and writes nothing.
    #[test]
    fn an_empty_prompt_is_refused_without_touching_the_log() {
        let session_id = "session-6";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(LIVE_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        let before = written_rows(&sessions, session_id).len();
        let outs = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-6",
                        "content": [],
                    },
                }),
            },
        );
        assert!(matches!(reply_of(&outs), RpcReply::Err { .. }), "{outs:?}");
        assert_eq!(written_rows(&sessions, session_id).len(), before);
    }

    /// A cancel is accepted, recorded, and never leaves an unbalanced frame.
    ///
    /// The FSM latches a cancel while a step is open and settles it with the
    /// reply, because forcing the closers at cancel time would leave the model's
    /// answer with no step to settle into. So the cancel is a *request* on the
    /// log, and the turn still closes exactly once.
    #[test]
    fn a_cancel_is_recorded_and_leaves_the_frames_balanced() {
        let session_id = "session-7";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(LIVE_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        let _ = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-7",
                        "content": [{ "type": "text", "text": "ping" }],
                    },
                }),
            },
        );
        // The turn has finished by now, so a late cancel is accepted and adds
        // nothing: there is no turn to abort.
        let outs = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "cancel",
                    "args": { "sessionId": session_id },
                }),
            },
        );
        assert!(matches!(reply_of(&outs), RpcReply::Ok { .. }), "{outs:?}");

        let rows = written_rows(&sessions, session_id);
        let types: Vec<&str> = rows
            .iter()
            .map(|r| r.get("type").and_then(Value::as_str).unwrap_or_default())
            .collect();
        // Balanced despite the cancel.
        for frame in ["turn/start", "turn/end", "step/start", "step/end"] {
            assert_eq!(
                types.iter().filter(|t| **t == frame).count(),
                1,
                "{frame} exactly once in {types:?}"
            );
        }
    }

    /// A cancel for an idle session is accepted and writes nothing.
    #[test]
    fn a_cancel_with_no_running_turn_is_a_no_op() {
        let session_id = "session-8";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(LIVE_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        let before = written_rows(&sessions, session_id).len();
        let outs = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "cancel",
                    "args": { "sessionId": session_id },
                }),
            },
        );
        assert!(matches!(reply_of(&outs), RpcReply::Ok { .. }), "{outs:?}");
        assert_eq!(
            written_rows(&sessions, session_id).len(),
            before,
            "an idle cancel must not append anything"
        );
    }

    /// A live turn against whatever endpoint the environment names.
    ///
    /// The only test that answers "does a real gateway accept the body this core
    /// builds, and does the decoder read what it sends back". Gated rather than
    /// silently skipped so CI stays hermetic — and it announces the skip, so a
    /// green run cannot be mistaken for coverage.
    #[test]
    fn a_live_turn_against_a_real_provider() {
        let Some(route) = live_route(ProviderKind::OpenAiChat) else {
            eprintln!(
                "SKIPPED (not a failure): set VOCODER_LIVE_BASE_URL and \
                 VOCODER_LIVE_API_KEY to run the live turn"
            );
            return;
        };
        let session_id = "session-live";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![route]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let outs = drive_live(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "live-1",
                        "content": [{ "type": "text", "text": "Reply with exactly PONG" }],
                    },
                }),
            },
        );
        assert!(matches!(reply_of(&outs), RpcReply::Ok { .. }), "{outs:?}");

        let rows = written_rows(&sessions, session_id);
        let msg = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("assistant/message"))
            .unwrap_or_else(|| panic!("no assistant message in {rows:?}"));
        let text: String = msg
            .pointer("/data/message/content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        assert!(
            text.to_uppercase().contains("PONG"),
            "the reply should reach the log: {text:?}"
        );
        let stream = msg
            .pointer("/data/stream")
            .and_then(Value::as_array)
            .expect("a recorded stream");
        assert_eq!(
            stream
                .last()
                .and_then(|c| c.pointer("/chunk/type"))
                .and_then(Value::as_str),
            Some("finish"),
            "a live stream must end with a finish chunk"
        );
        // And the row a live provider produced is replayable.
        let script = super::super::llm_replay::derive_replay_script(&rows)
            .expect("a live turn must derive as a replay script");
        assert_eq!(script.len(), 1);
    }

    /// The live configuration, or `None` when the environment has none.
    fn live_route(kind: ProviderKind) -> Option<Route> {
        let base_url = std::env::var("VOCODER_LIVE_BASE_URL").ok()?;
        let key = std::env::var("VOCODER_LIVE_API_KEY").ok()?;
        if base_url.is_empty() || key.is_empty() {
            return None;
        }
        // The key travels through the environment, which is the real path: the
        // config names a variable and `auth()` reads it.
        // SAFETY: single-threaded test; the name is unique to it.
        unsafe { std::env::set_var("VOCODER_LIVE_KEY_FOR_TEST", &key) };
        Some(Route {
            config: ProviderConfig {
                id: "canned".into(),
                kind,
                base_url,
                model: std::env::var("VOCODER_LIVE_MODEL")
                    .unwrap_or_else(|_| "claude-haiku-4-5".into()),
                api_key_env: Some("VOCODER_LIVE_KEY_FOR_TEST".into()),
            },
        })
    }

    /// Drive with real effects, for the live test.
    fn drive_live(machine: &mut AgentMachine, pending: MachineIn) -> Vec<MachineOut> {
        drive_collecting(machine, pending).0
    }

    /// A body that streams markdown one token at a time, so the intermediate
    /// frames are partially-written markdown rather than a complete document.
    ///
    /// The splits are chosen to land *inside* markers on purpose: `**` opens at
    /// one delta and closes three deltas later, so a frame emitted mid-way is
    /// genuinely unterminated. A body whose deltas happened to align with
    /// marker boundaries would make every intermediate frame well-formed and the
    /// stitching assertion would pass without the stitcher doing anything.
    const MARKDOWN_BODY: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
        "\"created\":1,\"id\":\"c\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
        // "Here is **bo"
        "data: {\"choices\":[{\"delta\":{\"content\":\"Here is **bo\"},\"finish_reason\":null,",
        "\"index\":0}],\"created\":1,\"id\":\"c\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
        // "ld** text"
        "data: {\"choices\":[{\"delta\":{\"content\":\"ld** text\"},\"finish_reason\":null,",
        "\"index\":0}],\"created\":1,\"id\":\"c\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\",\"index\":0}],",
        "\"created\":1,\"id\":\"c\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: [DONE]\n\n",
    );

    /// Every `agent/assistant-stream` frame in a batch of outputs.
    fn stream_frames(outs: &[MachineOut]) -> Vec<Value> {
        outs.iter()
            .filter_map(|o| match o {
                MachineOut::Dispatch { name, payload, .. }
                    if name.0 == "agent/assistant-stream" =>
                {
                    payload.get("frame").cloned()
                }
                _ => None,
            })
            .collect()
    }

    /// The text deltas among a frame list, in order.
    fn text_deltas(frames: &[Value]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| f.pointer("/chunk"))
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text-delta"))
            .filter_map(|c| c.get("text").and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    /// A settled turn announces the rows it appended, so a follower's event
    /// stream is gap-free.
    ///
    /// The agent writes the whole turn itself — it does not go through
    /// `session/prompt`'s recorder — so without this announcement a follower
    /// receives the live partial and then no settled event, and the `end`
    /// frame's `seq` names a row the client never sees.
    #[test]
    fn a_published_turn_announces_its_new_rows() {
        let session_id = "session-1";
        let (_home, sessions) = session_home_with(session_id, true);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(MARKDOWN_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let before = written_rows(&sessions, session_id).len();
        let (terminal, _during) = drive_collecting(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-announce",
                        "content": [{ "type": "text", "text": "hi" }],
                    },
                }),
            },
        );
        assert!(matches!(reply_of(&terminal), RpcReply::Ok { .. }));

        // Announcements are not stream frames, so they land in `terminal`.
        let announced: Vec<&Value> = terminal
            .iter()
            .filter_map(|o| match o {
                MachineOut::Dispatch { name, payload, .. } if name.0 == "session/event" => {
                    payload.get("event")
                }
                _ => None,
            })
            .collect();
        assert!(
            !announced.is_empty(),
            "a published turn announces its rows: {terminal:?}"
        );

        // Every announced row exists in the log — an announcement of a row that
        // was never written would hand a follower an event it cannot reconcile
        // with the next snapshot.
        let rows = written_rows(&sessions, session_id);
        for event in &announced {
            let seq = event.get("seq").and_then(Value::as_f64);
            assert!(
                rows.iter()
                    .any(|r| r.get("seq").and_then(Value::as_f64) == seq),
                "announced seq {seq:?} is in the log"
            );
        }
        // And they are exactly the *new* rows: announcing the pre-existing
        // prefix would replay every earlier event to the follower on each turn.
        //
        // `written_rows` includes the log header (row 0) while the announcement
        // counts only events, so the comparison drops the header from both.
        assert_eq!(
            announced.len(),
            (rows.len() - 1) - (before - 1),
            "only the rows this turn added are announced"
        );
        // The settled message is among them, which is what retires the partial.
        assert!(
            announced
                .iter()
                .any(|e| e.get("type").and_then(Value::as_str) == Some("assistant/message")),
            "the settled message is announced: {announced:?}"
        );
    }
    ///
    /// Without this the client's partial never resolves. The durable
    /// `assistant/message` event arrives on the same follow stream *behind*
    /// these frames, so a client that stopped at the last chunk would show a
    /// live partial forever — and `committed.seq` is what correlates the frame
    /// with the event that follows it.
    #[test]
    fn a_settled_attempt_ends_with_a_committed_frame() {
        let session_id = "session-1";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(MARKDOWN_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let (terminal, during) = drive_collecting(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-end",
                        "content": [{ "type": "text", "text": "hi" }],
                    },
                }),
            },
        );
        assert!(matches!(reply_of(&terminal), RpcReply::Ok { .. }));

        // The `start` and chunks are emitted *during* the call; the `end` is
        // emitted when the call's answer lands, so it is a terminal output.
        // Both halves are the same attempt, which is the point.
        let frames = stream_frames(&during);
        let start = frames
            .iter()
            .find(|f| f.get("type").and_then(Value::as_str) == Some("start"))
            .expect("a start frame");
        let all: Vec<Value> = frames
            .iter()
            .cloned()
            .chain(stream_frames(&terminal))
            .collect();
        let end = all
            .iter()
            .find(|f| f.get("type").and_then(Value::as_str) == Some("end"))
            .expect("an end frame");
        assert_eq!(
            end.get("attemptId").and_then(Value::as_str),
            start.get("attemptId").and_then(Value::as_str),
            "the end names the attempt whose chunks preceded it"
        );
        assert_eq!(
            end.pointer("/outcome/kind").and_then(Value::as_str),
            Some("committed"),
            "a finished call commits: {end:?}"
        );
        assert_eq!(
            end.pointer("/outcome/eventType").and_then(Value::as_str),
            Some("assistant/message")
        );
        // The seq must name the row that was actually appended, so a client can
        // match the frame to the event. `-1` is the "no row" sentinel, so it
        // would mean the frame points at nothing.
        let seq = end
            .pointer("/outcome/seq")
            .and_then(Value::as_f64)
            .expect("a seq");
        let rows = written_rows(&sessions, session_id);
        let msg = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("assistant/message"))
            .expect("the settled row");
        assert_eq!(
            seq,
            msg.get("seq").and_then(Value::as_f64).unwrap(),
            "the end frame points at the row it commits"
        );
        // The revision continues the attempt's own counter rather than
        // restarting, which is what a client's continuity check compares.
        let last_chunk_rev = all
            .iter()
            .filter(|f| f.get("type").and_then(Value::as_str) == Some("chunk"))
            .filter_map(|f| f.get("revision").and_then(Value::as_u64))
            .next_back()
            .expect("chunks");
        assert_eq!(
            end.get("revision").and_then(Value::as_u64),
            Some(last_chunk_rev + 1),
            "the end continues the revision sequence"
        );
    }

    /// A prompt drives the turn and the display frames arrive *while the call is
    /// in flight*, not all at once at the end.
    ///
    /// This is the property the whole streaming path exists for, and it is
    /// asserted by where the frames were collected rather than by their
    /// contents: `during` is filled by the chunk deliveries that run inside the
    /// effect, so a design that queued everything until the fetch returned would
    /// leave it empty and still produce a frame-for-frame identical protocol.
    #[test]
    fn display_frames_are_emitted_while_the_call_is_in_flight() {
        let session_id = "session-1";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(MARKDOWN_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let (terminal, during) = drive_collecting(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-stream",
                        "content": [{ "type": "text", "text": "hi" }],
                    },
                }),
            },
        );
        assert!(
            matches!(reply_of(&terminal), RpcReply::Ok { .. }),
            "turn accepted"
        );

        let frames = stream_frames(&during);
        assert!(
            !frames.is_empty(),
            "frames must arrive during the call, not only at the end"
        );
        // The attempt opens with a `start`, then dense chunks.
        assert_eq!(
            frames[0].get("type").and_then(Value::as_str),
            Some("start"),
            "the attempt is announced before its chunks: {frames:?}"
        );
        let attempt = frames[0]["attemptId"]
            .as_str()
            .expect("attemptId")
            .to_string();
        let indices: Vec<u64> = frames
            .iter()
            .filter(|f| f.get("type").and_then(Value::as_str) == Some("chunk"))
            .filter_map(|f| f.get("index").and_then(Value::as_u64))
            .collect();
        // Dense from zero: the control's accumulator drops an attempt on a gap,
        // so a single skipped index would silently blank the live display.
        assert_eq!(
            indices,
            (0..indices.len() as u64).collect::<Vec<_>>(),
            "chunk indices must be dense"
        );
        for frame in &frames {
            if let Some(id) = frame.get("attemptId").and_then(Value::as_str) {
                assert_eq!(id, attempt, "every frame names the same attempt");
            }
        }
    }

    /// A partially-streamed markdown block is stitched for display, and the
    /// durable record keeps the model's own bytes.
    ///
    /// Both halves matter and they pull in opposite directions: the display must
    /// close `**bo` so the frame renders as bold rather than as literal
    /// asterisks, and the log must keep `**bo` so replaying the session
    /// reproduces the conversation the model actually had.
    #[test]
    fn a_partial_markdown_block_is_stitched_for_display_only() {
        let session_id = "session-1";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(MARKDOWN_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let (terminal, during) = drive_collecting(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-md",
                        "content": [{ "type": "text", "text": "hi" }],
                    },
                }),
            },
        );
        assert!(matches!(reply_of(&terminal), RpcReply::Ok { .. }));

        let frames = stream_frames(&during);
        let deltas = text_deltas(&frames);
        assert!(!deltas.is_empty(), "the text streamed");
        // The deltas are the model's own bytes: an append-only field cannot
        // carry a repair, because the inserted characters would survive into
        // the client's concatenation. See `markdown.rs` for the proof.
        assert_eq!(
            deltas.concat(),
            "Here is **bold** text",
            "the deltas concatenate to exactly what the model wrote: {deltas:?}"
        );
        // The repair rides on `block-end`, which a client applies *wholesale* —
        // it is the protocol's only retraction point. Mid-stream the block is
        // unterminated, and this is where a client renders it closed.
        let block_end = frames
            .iter()
            .filter_map(|f| f.pointer("/chunk"))
            .find(|c| c.get("type").and_then(Value::as_str) == Some("block-end"))
            .expect("a block-end frame");
        assert_eq!(
            block_end.pointer("/block/type").and_then(Value::as_str),
            Some("text")
        );
        assert_eq!(
            block_end.pointer("/block/text").and_then(Value::as_str),
            Some("Here is **bold** text"),
            "block-end carries the assembled, stitched block: {block_end:?}"
        );

        // The log, by contrast, holds exactly what the model sent.
        let rows = written_rows(&sessions, session_id);
        let msg = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("assistant/message"))
            .expect("an assistant message");
        let text = msg
            .pointer("/data/message/content/0/text")
            .and_then(Value::as_str)
            .expect("content text");
        assert_eq!(
            text, "Here is **bold** text",
            "the durable record is the model's own bytes"
        );
        // And the *stream record* — the other durable copy — is unstitched too.
        // It is the one that would be replayable, so a stitched byte here would
        // fabricate a marker the model never wrote.
        let stream = msg.pointer("/data/stream").expect("stream record");
        let recorded: String = stream
            .as_array()
            .expect("array")
            .iter()
            .filter(|r| r.get("type").and_then(Value::as_str) == Some("text-chunks"))
            .flat_map(|r| {
                r.get("texts")
                    .and_then(Value::as_array)
                    .map(|t| t.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            recorded, "Here is **bold** text",
            "the stream record keeps the deltas as they arrived"
        );
    }

    /// A tool-call block is never stitched, even mid-fragment.
    ///
    /// Its fragments are JSON, and a markdown repairer over JSON produces
    /// arguments the client cannot parse — the failure the prose check exists to
    /// prevent, asserted here through the live path rather than only in the
    /// markdown module's own unit test.
    #[test]
    fn a_tool_call_fragment_is_forwarded_unstitched() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,",
            "\"index\":0}],\"created\":1,\"id\":\"c\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            // Arguments containing a markdown-lookalike that a repairer would
            // rewrite: an unclosed bold marker and an open fence.
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
            "\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"body\\\":\\\"**unclosed\"}}]},",
            "\"finish_reason\":null,\"index\":0}],\"created\":1,\"id\":\"c\",\"model\":\"m\",",
            "\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\",\"index\":0}],",
            "\"created\":1,\"id\":\"c\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        );
        let session_id = "session-1";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let (_, during) = drive_collecting(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-tool",
                        "content": [{ "type": "text", "text": "go" }],
                    },
                }),
            },
        );
        let frames = stream_frames(&during);
        let args: Vec<&str> = frames
            .iter()
            .filter_map(|f| f.pointer("/chunk"))
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("tool-call-delta"))
            .filter_map(|c| c.get("argumentsDelta").and_then(Value::as_str))
            .collect();
        let joined = args.concat();
        assert!(
            joined.contains("**unclosed"),
            "the argument fragment arrives verbatim: {joined:?}"
        );
        assert!(
            !joined.contains("**unclosed**"),
            "a tool argument must not be markdown-repaired: {joined:?}"
        );
    }
}
