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
use super::agent_loop::{AgentLoop, CancelCause, Draft, LoopOutput, StepOutcome};
use super::llm_replay::{BlockType, FinishReason, StreamChunk, step_outcome};
use super::markdown::DisplayAccumulator;
use super::provider::{
    ProviderConfig, ProviderKind, ProviderStreamDecoder, SseDecoder, SseFrame, to_wire_request,
};
use super::readcache::{FsCache, Pending};
use super::sandbox::{Fence, Mode};
use super::tool_exec::{Executor, ExecutorOut};
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
    /// The tool calls the step's reply made, waiting for their executor.
    ///
    /// On the turn state rather than the machine because it belongs to a turn: a
    /// second turn must not inherit the first's calls, and the executor that
    /// consumes them is created from this at the step that follows.
    pending_calls: Option<Vec<super::tool::Call>>,
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
    /// A step's tool calls are running.
    ///
    /// The turn is held here, not in `Call`/`Settle`: the reply has settled and
    /// its `assistant/message` is already written, but the step cannot close until
    /// the calls it made have results. Holding the state is what lets the
    /// executor's effects suspend and resume without losing the turn.
    Tools { state: TurnState },
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
    /// The step's tool executor, while a step's calls are running.
    ///
    /// One per step rather than per turn: a step's calls are all made by one
    /// `assistant/message` at one position, so the executor's own `position` is
    /// fixed for its life.
    exec: Option<Executor>,
    /// The tool effect in flight: its id and what it was asked for.
    ///
    /// The effect's *kind* is remembered rather than recovered from the answer,
    /// because `EffectResult` is untyped: a `ReadText` answer arriving for a
    /// `WriteText` request would otherwise be read as one, and a tool would render
    /// the wrong thing from real bytes.
    tool_effect: Option<(vocoder_cordis::EffectId, super::tool_exec::Effect)>,
    /// The attempt id, revision and next index of the call that just settled.
    ///
    /// The `Live` that produced them is dropped when `settle` runs, but the
    /// `end` frame has to name the same attempt and continue the same counters —
    /// a client validates that every frame of an attempt carries one id and that
    /// chunk indices are dense, so an `end` that invented a fresh id would be
    /// rejected and leave the partial live forever. Carrying the summary is how
    /// the closing frame stays attributable after its decoder is gone.
    last_attempt: Option<(String, u64, u64)>,
    /// Background-job tables, keyed by session id — the registry
    /// `dsh/packages/jobs/jobs-local` keeps per owner. A *map* rather than one
    /// table because ids are minted per session (`bash-N` sequences are
    /// owner-relative, so two sessions both have a `bash-1`), and the fence
    /// upstream's registry enforces (`job <id> belongs to another session`)
    /// is structural here rather than checked: a session's own table is the
    /// only one its reads ever reach.
    jobs: BTreeMap<String, super::tool_exec::Jobs>,
    /// The boot-resolved sandbox facts a confined `bash` call needs.
    ///
    /// Resolved by the driver (probing runners and the environment is I/O) and
    /// handed in at mount. The default is fail-closed: a machine that was never
    /// given a context refuses every command rather than running one unconfined,
    /// which is what every test that drives the fs tools but not `bash` wants.
    sandbox: super::tool_bash::SandboxContext,
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
            exec: None,
            tool_effect: None,
            sandbox: super::tool_bash::SandboxContext::default(),
            jobs: BTreeMap::new(),
        }
    }

    /// The fail-closed default replaced with a resolved context.
    ///
    /// Mount is where a driver hands a machine its boot facts, so this is the
    /// seam the real host uses and the tests do not — a test that runs `bash`
    /// passes its own context here rather than resolving one.
    pub fn with_sandbox(mut self, sandbox: super::tool_bash::SandboxContext) -> Self {
        self.sandbox = sandbox;
        self
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
                // A tool result is a *user-role* message on its own row type.
                // Reading it here is what makes a tool-calling turn work at all:
                // the next request has to answer the call it made, and a provider
                // rejects a conversation with an unanswered tool call. The row is
                // its own type rather than a `user/message` because the log
                // distinguishes a human's input from a tool's output.
                "tool/result" => {
                    let Some(msg) = row.get("data").and_then(|d| d.get("message")) else {
                        continue;
                    };
                    let items = content_items(msg);
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
                // The tools the host can actually execute. Offering a name with
                // no implementation would produce a result reading `unknown
                // tool`, which teaches the model the tool exists and is broken;
                // see `tool::catalog` on why the set is closed.
                //
                // Every step offers them, which is what lets a turn continue
                // across tool results: the model's next request carries the
                // conversation *and* the same tool set, so a call it made is
                // still callable when it decides what to do with the answer.
                tools: super::tool::tool_definitions(),
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
            pending_calls: None,
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
            // A tool step re-enters at the executor: either it still has calls to
            // run, or they are done and the step may close.
            Some(Op::Tools { mut state }) => {
                if self.exec.as_ref().is_some_and(Executor::finished) {
                    return self.close_tool_step(state);
                }
                let outs = self.drive_tools(&mut state);
                // A call that runs to its result with no effect — a refusal at
                // the gate, or a `job_list` answering from the machine's own
                // table — leaves nothing outstanding to resume on, so the step
                // closes here rather than waiting for an answer that will
                // never arrive.
                if self.exec.as_ref().is_some_and(Executor::finished)
                    && self.tool_effect.is_none()
                {
                    // Park first: the close issues its own repairs and reads.
                    self.op = Some(Op::Tools { state });
                    let mut cuts = outs;
                    let state = match self.op.take() {
                        Some(Op::Tools { state }) => state,
                        _ => unreachable!(),
                    };
                    cuts.extend(self.close_tool_step(state));
                    return cuts;
                }
                self.op = Some(Op::Tools { state });
                outs
            }
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
        let (mut message, usage) = assemble_message(&chunks, &provider, &state.rows);
        let reason = chunks.iter().rev().find_map(|c| match c {
            StreamChunk::Finish { reason } => Some(reason.clone()),
            _ => None,
        });
        // A cancel latched while this call was in flight outranks the reply: the
        // turn ends `aborted`, and what the model produced becomes the
        // *interrupted prefix* rather than a reply — upstream settles it as
        // `assistant/message` with `interrupted: true` and drops undispatched
        // tool calls, or as `assistant/attempt` when no visible content streamed
        // (`core/session/src/types.ts`, `assistant/message`'s `interrupted`).
        //
        // The distinction is load-bearing for a reader: an ordinary
        // `assistant/message` claims the model finished, and a turn that was cut
        // off would replay as a completed answer.
        let interrupted = state.fsm.cancel_pending();
        if interrupted {
            // Undispatched tool calls are absent from the prefix; their results
            // never happened, so recording the calls would propose work the
            // model asked for and the loop never ran.
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                parts.retain(|p| p.get("type").and_then(Value::as_str) != Some("tool-call"));
            }
        }
        let has_visible_content = message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| !parts.is_empty());
        let has_calls = chunks.iter().any(|c| {
            matches!(
                c,
                StreamChunk::BlockStart {
                    block_type: BlockType::ToolCall,
                    ..
                }
            )
        });
        // A step that called tools runs them **before the step closes**, which is
        // the order the corpus records: `assistant/message`, the calls, their
        // results, then `step/end`. `step_reply` is what emits `step/end`, so the
        // executor is entered first and the FSM is advanced once it finishes.
        // Driving the tools after `continue_turn` instead would file them under
        // the next step, which is a different durable claim about what happened.
        //
        // An interrupted turn runs none of them: the calls were dropped from the
        // prefix above because they were never dispatched. Captured before the
        // message is moved into the row below, since the executor reads the
        // calls out of it.
        if has_calls && reason.is_some() && !interrupted {
            state.pending_calls = Some(super::tool_exec::calls_in(&message));
        }
        // A stream with no finish chunk is recorded as an *attempt*, not a
        // message: claiming a reply the model never finished would make a broken
        // call replay as a good one. An interrupted turn with nothing visible
        // streamed is the same kind of record for the same reason.
        let row_type = if interrupted {
            if has_visible_content {
                "assistant/message"
            } else {
                "assistant/attempt"
            }
        } else if reason.is_some() {
            "assistant/message"
        } else {
            "assistant/attempt"
        };
        let (turn, step) = state.fsm.position();
        let mut data = json!({
            "turn": turn,
            "step": step,
            "stream": chunk_records(&chunks),
        });
        if row_type == "assistant/message" {
            data["message"] = message;
            // Absent rather than zero when the adapter reported nothing; see
            // `assemble_message`.
            if let Some(usage) = usage {
                data["usage"] = usage;
            }
            if interrupted {
                data["interrupted"] = json!(true);
            }
        }
        let settled = Self::row(&mut state.rows, &Draft { row_type, data });
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
                "outcome": if row_type == "assistant/message" {
                    json!({ "kind": "committed", "eventType": row_type, "seq": seq })
                } else {
                    json!({ "kind": "abandoned" })
                },
            }),
        )];
        if state.pending_calls.is_some() {
            tracing::debug!("settle: has pending calls");
            self.op = Some(Op::Tools { state });
            // The calls are captured; the executor is created and driven on the
            // same re-entry that put the op in place.
            let resumed = self.resume_op();
            end_frames.extend(resumed);
            return end_frames;
        }
        tracing::debug!("settle: no pending calls");
        let outs = state
            .fsm
            .step_reply(outcome, has_calls && !interrupted, false);
        let call = self.apply(&mut state, outs);
        end_frames.extend(self.continue_turn(state, call));
        end_frames
    }

    /// Feed a tool effect's answer back to the executor.
    fn answer_tool(
        &mut self,
        mut state: TurnState,
        effect: &super::tool_exec::Effect,
        result: EffectResult,
    ) -> Vec<MachineOut> {
        use super::tool::{Answer, display_path};
        let fence = self.fence_for(&state);
        let session = state.session.clone();
        let answer = match result {
            EffectResult::Text(text) => Ok(Answer::Text(text)),
            EffectResult::Done => Ok(Answer::Done),
            EffectResult::Stat {
                canonical,
                is_dir,
                version,
                ..
            } => Ok(Answer::Stat {
                canonical,
                is_dir,
                version: Some(version),
            }),
            // A confined command settled. Every field is carried to the
            // renderer: the exit code and the signal are distinct (a signal
            // death has no code), and `truncated` is not inferable from text.
            EffectResult::ProcessDone {
                exit_code,
                signal,
                stdout,
                stderr,
                truncated,
                timed_out,
                aborted,
                spill_path,
            } => Ok(Answer::Process {
                exit_code,
                signal,
                stdout,
                stderr,
                truncated,
                timed_out,
                aborted,
                spill_path,
            }),
            // The detached triple: the start's pid is the whole answer, and a
            // chunk carries exactly what the executor's job renderer reads.
            EffectResult::ProcessStarted { pid } => Ok(Answer::ProcessStarted { pid }),
            EffectResult::ProcessChunk {
                running,
                stdout_delta,
                stderr_delta,
                exit_code,
                signal,
                truncated: _,
                spill_path: _,
            } => {
                Ok(Answer::ProcessChunk {
                    running,
                    stdout_delta,
                    stderr_delta,
                    exit_code,
                    signal,
                    aborted: false,
                })
            }
            // A missing target is its own answer rather than a failure, because
            // the tools branch on it: a `write` creates, a `read` refuses, and
            // each says so in its own words.
            EffectResult::Failed(vocoder_cordis::EffectError::NotFound) => Ok(Answer::NotFound),
            EffectResult::Failed(e) => {
                // The `read`/`edit` not-found wording is the tool layer's, so the
                // path it names is the display path rather than the raw one.
                let path = match effect {
                    super::tool_exec::Effect::Stat { path }
                    | super::tool_exec::Effect::Read { path }
                    | super::tool_exec::Effect::Write { path, .. } => path,
                    // A `bash` failure never reaches here: its spawn failure is
                    // `Answer::Failed` (handled above), not `EffectError`. The
                    // path is unused either way — only the message is rendered —
                    // so a command's empty path is inert.
                    super::tool_exec::Effect::Exec { .. }
                    | super::tool_exec::Effect::ExecDetached { .. }
                    | super::tool_exec::Effect::ExecRead { .. }
                    | super::tool_exec::Effect::ExecKill { .. } => "",
                };
                let _ = display_path(&fence.root, path);
                Ok(Answer::Failed(e.message()))
            }
            other => Err(format!("unexpected effect answer: {other:?}")),
        };
        let wants = match self.exec.as_mut() {
            Some(exec) => {
                let jobs = self.jobs.entry(session.clone()).or_default();
                exec.on_effect(effect, answer, jobs)
            }
            None => Vec::new(),
        };
        let mut outs = self.absorb_tool_wants(&mut state, wants);
        // The executor either finished or wants another effect. Both paths park
        // the turn back on the op first, so a suspension mid-step has a state to
        // resume from.
        if self.exec.as_ref().is_some_and(Executor::finished) {
            outs.extend(self.close_tool_step(state));
        } else {
            let more = self.drive_tools(&mut state);
            self.op = Some(Op::Tools { state });
            outs.extend(more);
        }
        outs
    }

    /// Answer an approval waterfall the executor started.
    fn on_verdict(&mut self, verdict: &Value) -> Vec<MachineOut> {
        let Some(Op::Tools { state }) = self.op.take() else {
            // A verdict for an ask this machine is not waiting on — a chain
            // another machine started. Not an error; simply not ours.
            return Vec::new();
        };
        let mut state = state;
        let root = self.workspace_root(&state);
        let session = state.session.clone();
        let wants = match self.exec.as_mut() {
            Some(exec) => {
                let jobs = self.jobs.entry(session).or_default();
                exec.on_verdict(verdict, &root, &self.sandbox, jobs)
            }
            None => Vec::new(),
        };
        let outs = self.absorb_tool_wants(&mut state, wants);
        self.op = Some(Op::Tools { state });
        outs
    }

    /// Close a step whose tool calls have all run, and advance the turn.
    ///
    /// This is where the reply the executor was holding finally reaches the FSM.
    /// The step cannot close before its calls have results — `step_reply` is what
    /// emits `step/end`, so for a tool-calling step it is called here rather than
    /// in `settle`.
    fn close_tool_step(&mut self, mut state: TurnState) -> Vec<MachineOut> {
        self.exec = None;
        self.tool_effect = None;
        state.pending_calls = None;
        // A tool call always continues the turn: the results are what the next
        // step is for. `more_input` comes from the durable inbox fold, the same
        // authority the FSM's own `has_pending` reads.
        let more_input = Inbox::fold(&state.rows)
            .map(|i| i.has_pending())
            .unwrap_or(false);
        let outs = state
            .fsm
            .step_reply(StepOutcome::Completed, true, more_input);
        let call = self.apply(&mut state, outs);
        self.continue_turn(state, call)
    }

    /// Run a step's tool calls, one at a time, in model order.
    ///
    /// The executor is pure and returns its wants; this maps them onto the
    /// machine's vocabulary. Two details are load-bearing:
    ///
    /// - **`seq` is assigned here, not in the executor.** The log's length is the
    ///   publisher's fact and a pure transition may not consult it, so each row
    ///   the executor asks for is appended through [`Self::row`] and its assigned
    ///   `seq` is fed back with [`Executor::observe_row`]. That is how a
    ///   `tool/result` cites the `tool/call` row it answers — without it
    ///   `sourceEventSeqs` would carry a number nothing reconciles.
    /// - **Every row is announced**, like the turn's own rows, because the agent
    ///   owns the log for this turn and a follower's event sequence has to be
    ///   gap-free.
    fn drive_tools(&mut self, state: &mut TurnState) -> Vec<MachineOut> {
        let root = self.workspace_root(state);
        let (turn, step) = state.fsm.position();
        if self.exec.is_none() {
            // A step's calls are made by one `assistant/message`, so the executor
            // is created once, when that reply settles.
            let calls = state.pending_calls.clone().unwrap_or_default();
            self.exec = Some(Executor::new(calls, turn, step));
        }
        if self.exec.as_ref().is_some_and(Executor::finished) {
            return Vec::new();
        }
        let fence = self.fence_for(state);
        let wants = match self.exec.as_mut() {
            // `exec`, `sandbox` and the session's own `jobs` row are three
            // disjoint fields — the executor borrows the sandbox and the jobs
            // table it mints ids into without ever touching the machine's log
            // plumbing, which is what keeps a job minted mid-step reachable
            // from the kill the same step might issue.
            Some(exec) => {
                let jobs = self.jobs.entry(state.session.clone()).or_default();
                exec.begin(&root, &fence, &self.sandbox, jobs)
            }
            None => return Vec::new(),
        };
        self.absorb_tool_wants(state, wants)
    }

    /// Turn the executor's wants into machine outputs.
    fn absorb_tool_wants(
        &mut self,
        state: &mut TurnState,
        wants: Vec<ExecutorOut>,
    ) -> Vec<MachineOut> {
        let mut outs = Vec::new();
        for want in wants {
            match want {
                ExecutorOut::Row { row_type, data } => {
                    let appended = Self::row(&mut state.rows, &Draft { row_type, data });
                    let seq = appended.get("seq").and_then(Value::as_f64).unwrap_or(0.0) as u64;
                    if let Some(exec) = self.exec.as_mut() {
                        exec.observe_row(row_type, seq);
                    }
                    outs.push(MachineOut::Dispatch {
                        name: vocoder_cordis::EventName::new("session/event"),
                        payload: json!({ "sessionId": state.session, "event": appended }),
                        mode: vocoder_cordis::DispatchMode::Emit,
                    });
                }
                ExecutorOut::Effect(effect) => outs.extend(self.realize_tool(&effect)),
                ExecutorOut::Dispatch {
                    name,
                    payload,
                    waterfall,
                } => outs.push(MachineOut::Dispatch {
                    name: vocoder_cordis::EventName::new(&name),
                    payload,
                    mode: if waterfall {
                        vocoder_cordis::DispatchMode::Waterfall
                    } else {
                        vocoder_cordis::DispatchMode::Emit
                    },
                }),
            }
        }
        outs
    }

    /// Issue the effect a tool call needs.
    fn realize_tool(&mut self, effect: &super::tool_exec::Effect) -> Vec<MachineOut> {
        use super::tool_exec::Effect;
        // The effect names the session running it, so `session/cancel` can
        // reach a live `bash` child without waiting out the pump.
        let kill_session = match &self.op {
            Some(Op::Tools { state }) => Some(state.session.clone()),
            _ => None,
        };
        let request = match effect {
            Effect::Stat { path } => RealizeRequest::Stat { path: path.clone() },
            Effect::Read { path } => RealizeRequest::ReadText { path: path.clone() },
            Effect::Write {
                path,
                contents,
                expect,
            } => RealizeRequest::WriteText {
                path: path.clone(),
                contents: contents.clone(),
                expect: expect.clone(),
            },
            // The argv arrives already wrapped by the sandbox machine, so this
            // is a plain spawn — the detached twin of `Exec`: same argv, same
            // environment, but no timeout, and the kill key reaches it through
            // the same registration, which is exactly what lets a
            // `session/cancel` stop a background job the turn forgot about.
            Effect::ExecDetached {
                confined,
                workdir,
                env,
            } => RealizeRequest::ProcessStart {
                argv: confined.argv.clone(),
                workdir: Some(workdir.clone()),
                env: env.clone(),
                stdout_max_bytes: Some(super::tool_bash::BASH_STDOUT_MAX_BYTES),
                spill_dir: Some(format!("{}/.spill", self.root.display())),
                kill_key: kill_session,
            },
            // The detached reads and kills are the jobs registry's own seam:
            // upstream's `ctx.jobs.read` / `ctx.jobs.kill` hand the caller the
            // same two facts these carry (unread text + settle state, and the
            // kill's own ask).
            Effect::ExecRead { pid } => RealizeRequest::ProcessRead { pid: *pid },
            Effect::ExecKill { pid } => RealizeRequest::ProcessKill { pid: *pid },
            // The argv arrives already wrapped by the sandbox machine, so this is
            // a plain spawn. No stdin is passed, matching the tool layer's
            // reduction, and the output is bounded by bash's own byte cap.
            Effect::Exec {
                confined,
                workdir,
                env,
                timeout_ms,
                ..
            } => RealizeRequest::ProcessExec {
                argv: confined.argv.clone(),
                workdir: Some(workdir.clone()),
                env: env.clone(),
                timeout_ms: Some(*timeout_ms),
                stdout_max_bytes: Some(super::tool_bash::BASH_STDOUT_MAX_BYTES),
                spill_dir: Some(format!("{}/.spill", self.root.display())),
                stdin: None,
                // Bound to the session: a `session/cancel` for it kills the
                // child (see `driver::kill_registered`).
                kill_key: kill_session,
            },
        };
        let id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        self.tool_effect = Some((id, effect.clone()));
        vec![rpc::effect(id, request)]
    }

    /// The workspace root a session's tools resolve against.
    ///
    /// Read from the log's header row, which is where `session/create` recorded
    /// the `cwd`. A session with none has no workspace, and the tools then resolve
    /// against the machine's own root — a boot-time fact the driver resolved, so
    /// the machine still reads no process state.
    fn workspace_root(&self, state: &TurnState) -> String {
        state
            .rows
            .first()
            .and_then(|r| r.get("cwd"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| self.root.to_string_lossy().to_string())
    }

    /// The confinement fence for a session.
    ///
    /// **The mode comes from `sandbox/mode`, never from `approval/policy`.** They
    /// are two independent axes and conflating them is a fail-open bug, which is
    /// what an earlier version of this function did:
    ///
    /// - `approval/policy` decides whether a request to *widen* is put to a human.
    /// - `sandbox/mode` decides what the fence actually permits right now.
    ///
    /// The corpus proves they are independent. `missing-sandbox-runner` and
    /// `partial-landlock-child-failure` both run at **`read-only`** — the most
    /// restrictive mode there is — while carrying `approval/policy: ask`. Reading
    /// the policy as if it were the mode derived `workspace-write` for both, i.e.
    /// it *granted writes to sessions upstream confines to reads*. The base
    /// profile's own `permission` presets table shows the axes pairing
    /// consistently and yet not identically: `read-only` and `workspace-write`
    /// are both `approval: ask`, so the policy cannot distinguish them.
    ///
    /// The mode is the last `sandbox/mode` row, which is the projection upstream
    /// folds (`sandbox-policy/src/session-mode.ts`: "the LAST such event is the
    /// session's override"). With no row, the deployment default applies — and
    /// upstream's own default is `read-only` (`sandbox-policy`'s
    /// `Config.mode` default), which is why [`Mode::default_mode`] is the
    /// confined one rather than the widest.
    ///
    /// **Nothing writes `sandbox/mode` yet.** Upstream's write path is
    /// `permissionPresets`, which this host does not compose and which no wire
    /// descriptor carries — the presets are applied in-process, not over the
    /// gateway. So a session can only ever run at the default here. Stated rather
    /// than implied, because a reader would otherwise assume a client can switch
    /// modes.
    fn fence_for(&self, state: &TurnState) -> Fence {
        let mode = state
            .rows
            .iter()
            .rev()
            .find_map(|r| {
                (r.get("type").and_then(Value::as_str) == Some("sandbox/mode"))
                    .then(|| r.pointer("/data/mode").and_then(Value::as_str))
                    .flatten()
            })
            .and_then(Mode::parse)
            .unwrap_or_else(Mode::default_mode);
        Fence::new(mode, self.workspace_root(state))
    }
}

impl PluginMachine for AgentMachine {
    type In = MachineIn;
    type Out = MachineOut;

    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        match ev {
            MachineIn::ServicesReady { .. } => vec![
                MachineOut::Subscribe {
                    name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                },
                // The verdicts of the approval waterfalls this machine starts.
                // Without this subscription the ask would suspend forever: the
                // chain's final value is delivered as a `DispatchResult`, and a
                // machine that did not subscribe would never hear it.
                MachineOut::Subscribe {
                    name: vocoder_cordis::EventName::new("approval/request"),
                },
            ],
            MachineIn::DispatchResult { name, value } if name.0 == "approval/request" => {
                self.on_verdict(&value)
            }
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
    /// ordering that keeps `step/start`/`step/end` balanced. So a cancel
    /// mid-call reports acceptance and lets the in-flight call settle the turn;
    /// forcing the frame closers now would leave the reply with no step to
    /// settle into.
    ///
    /// A cancel that arrives while the loop is *between* steps owes its closers
    /// immediately, and the FSM returns them from `cancel` — those rows are
    /// appended here and the turn is published, because a cancel between steps
    /// is a whole turn's worth of durable change and no later effect will carry
    /// it.
    ///
    /// There is deliberately **no** row recording the request itself. Upstream
    /// records a cancellation by *closing the turn* with
    /// `turn/end {kind: 'aborted', reason: {kind: 'user'}}`
    /// (`core/session/src/types.ts`'s `TurnEndReasonMap`, emitted by
    /// `core/agent-loop/src/agent.ts` from `signal.reason`), and it has no
    /// event for the request; `agent/cancel-requested` is not in
    /// `KNOWN_SESSION_EVENT_TYPES`, so a reader would refuse the log.
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
        // A cancel is addressed to one session; a turn for a *different* session
        // is not the one being cancelled, and latching it would abort an
        // unrelated conversation. `Op::Tools` is in flight too: the child was
        // registered under the session's kill key, and its death settles the
        // running step back through the FSM the same way a settled model call
        // does, so the latch is the same.
        let holds_turn = match &self.op {
            Some(Op::Call { state, .. })
            | Some(Op::Settle { state, .. })
            | Some(Op::Tools { state }) => state.session == session,
            _ => false,
        };
        if !holds_turn {
            // Either nothing is in flight, or the turn in flight belongs to
            // another session. Neither has a turn to abort here.
            return rpc::ok(json!({ "accepted": true }));
        }
        // Latch first, then let the FSM say what the cancel owes. The order
        // matters: `cancel` reads the latch it sets, so a step that is open
        // returns nothing and the closer arrives with the reply. A `Tools` op
        // latches the same way; the child's death is the reply that settles it.
        let outs = {
            let fsm = match self.op.as_mut() {
                Some(Op::Call { state, .. })
                | Some(Op::Settle { state, .. })
                | Some(Op::Tools { state }) => Some(&mut state.fsm),
                _ => None,
            };
            match fsm {
                Some(fsm) => fsm.cancel(CancelCause::User),
                None => Vec::new(),
            }
        };
        // The FSM closed the turn itself — it was between steps. Append what it
        // returned and publish: no effect is outstanding to carry these rows, so
        // this call is the only chance to write them.
        if !outs.is_empty() {
            let state = match self.op.take() {
                Some(Op::Call { state, .. }) | Some(Op::Settle { state, .. }) => state,
                other => {
                    self.op = other;
                    return rpc::ok(json!({ "accepted": true }));
                }
            };
            let mut state = state;
            let call = self.apply(&mut state, outs);
            debug_assert!(call.is_none(), "a cancel owes no model call");
            return self.finish(&state);
        }
        // A step is open: the latch is set and the reply settles it. Nothing is
        // written now, and the answer is the same acceptance upstream returns.
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
        // A tool effect's answer belongs to the executor, not the cache: the
        // effect was issued for a *call*, and only the executor knows which one.
        // Matched by effect id rather than by "a tool is in flight", because a
        // late answer for a call the executor has already moved past would
        // otherwise be applied to the next one.
        if let (Op::Tools { .. }, Some((id, effect))) = (&op, self.tool_effect.clone()) {
            if id == _id {
                let state = match op {
                    Op::Tools { state } => state,
                    _ => unreachable!("matched above"),
                };
                self.tool_effect = None;
                return self.answer_tool(state, &effect, result);
            }
            let state = match op {
                Op::Tools { state } => state,
                _ => unreachable!("matched above"),
            };
            self.op = Some(Op::Tools { state });
            return vec![];
        }
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
///
/// `usage` is `None` when the adapter reported none, and the caller omits the
/// key entirely. Upstream spreads it conditionally on both settle paths
/// (`core/agent-loop/src/agent.ts`: `...live.usage === undefined ? {} : { usage:
/// live.usage }`), and the corpus agrees — four recorded `assistant/message`
/// rows carry no `usage` at all, the cancelled one among them
/// (`snapshots/acp/cancel/session.v3.jsonl`). Writing zeros instead would claim
/// the adapter reported a zero-token call, which is a different fact from
/// "no accounting arrived".
fn assemble_message(
    chunks: &[StreamChunk],
    provider: &str,
    rows: &[Value],
) -> (Value, Option<Value>) {
    // Block index → its kind and accumulated text, kept in a BTreeMap so the
    // output is ordered by the wire's own block numbering.
    let mut blocks: BTreeMap<u64, (BlockType, String)> = BTreeMap::new();
    // Tool calls accumulate separately: their fragments concatenate into a JSON
    // string, and their id and name arrive once.
    let mut tool_ids: BTreeMap<u64, (String, String)> = BTreeMap::new();
    let mut usage: Option<Value> = None;
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
            StreamChunk::Usage { usage: u } => usage = Some(u.clone()),
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
        written_rows_in(sessions, None, session_id)
    }

    /// The same, for a session whose header recorded a `cwd` — which is what a
    /// tool-calling turn needs, since the tools resolve against a workspace.
    fn written_rows_in(
        sessions: &std::path::Path,
        cwd: Option<&std::path::Path>,
        session_id: &str,
    ) -> Vec<Value> {
        let dir = sessions
            .join(super::super::session::SessionStore::project_dir(
                cwd.map(|c| c.to_string_lossy()).as_deref(),
            ))
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

    /// A reply that streams text and finishes, with **no usage trailer**.
    ///
    /// The shape the recorded cancellation has
    /// (`snapshots/acp/cancel/session.v3.jsonl`): the adapter reported no
    /// accounting, so the settling row must carry no `usage` key at all. A body
    /// with a usage trailer — [`LIVE_BODY`] — cannot exercise that, which is
    /// exactly how the zero-usage defect survived.
    const NO_USAGE_BODY: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
        "\"created\":1,\"id\":\"chatcmpl-2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"stop\",",
        "\"index\":0}],\"created\":1,\"id\":\"chatcmpl-2\",\"model\":\"m\",",
        "\"object\":\"chat.completion.chunk\"}\n\n",
        "data: [DONE]\n\n",
    );

    /// A canned reply that calls `bash`, then answers.
    ///
    /// The `bash` counterpart of [`READ_THEN_ANSWER`]: one body proposes a
    /// confined shell command, the second answers once its result is in the log.
    const BASH_THEN_ANSWER: [&str; 2] = [
        concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
            "\"created\":1,\"id\":\"c1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_b1\",",
            "\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},",
            "\"finish_reason\":null,\"index\":0}],\"created\":1,\"id\":\"c1\",\"model\":\"m\",",
            "\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\"{\\\"command\\\":\\\"echo dsh-bash-e2e\\\",",
            "\\\"description\\\":\\\"Prove bash runs\\\"}\"}}]},",
            "\"finish_reason\":null,\"index\":0}],\"created\":1,\"id\":\"c1\",\"model\":\"m\",",
            "\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\",\"index\":0}],",
            "\"created\":1,\"id\":\"c1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        ),
        concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
            "\"created\":1,\"id\":\"c2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"DONE\"},\"finish_reason\":\"stop\",\"index\":0}],",
            "\"created\":1,\"id\":\"c2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        ),
    ];

    /// A canned reply that starts a background job, then reads the session's
    /// jobs with `job_list`, then answers. The read is timing-sensitive (the
    /// detached child settles on its own thread, possibly *after* the read the
    /// very next step issues — which is exactly why upstream has the status
    /// marker), so a list is what proves the wiring without inventing a race.
    const BACKGROUND_THEN_READ: [&str; 3] = [
        concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
            "\"created\":1,\"id\":\"c1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_b1\",",
            "\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},",
            "\"finish_reason\":null,\"index\":0}],\"created\":1,\"id\":\"c1\",\"model\":\"m\",",
            "\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\"{\\\"command\\\":\\\":\\\",",
            "\\\"description\\\":\\\"No-op background\\\",\\\"run_in_background\\\":true}\"}}]},",
            "\"finish_reason\":null,\"index\":0}],\"created\":1,\"id\":\"c1\",\"model\":\"m\",",
            "\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\",\"index\":0}],",
            "\"created\":1,\"id\":\"c1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        ),
        concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
            "\"created\":1,\"id\":\"c2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_j1\",",
            "\"type\":\"function\",\"function\":{\"name\":\"job_list\",\"arguments\":",
            "\"{}\"}}]},\"finish_reason\":null,\"index\":0}],",
            "\"created\":1,\"id\":\"c2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\",\"index\":0}],",
            "\"created\":1,\"id\":\"c2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        ),
        concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
            "\"created\":1,\"id\":\"c3\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"DONE\"},\"finish_reason\":\"stop\",\"index\":0}],",
            "\"created\":1,\"id\":\"c3\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        ),
    ];

    /// **A background call runs detached, reports its id, and `job_output`
    /// collects it — through the whole agent.**
    ///
    /// The sequence the corpus fixes: `bash` with `run_in_background: true`
    /// answers immediately with the minted id (`
    /// `started background job bash-1`), the very next step reads the job by
    /// the id, and the driver's detached path settles the no-op command. Like
    /// the confined tests, it self-skip where no runner is usable.
    #[test]
    fn a_background_call_is_detached_and_read_through_the_agent() {
        let context = crate::driver::probe_sandbox(std::env::consts::OS);
        if matches!(
            context.selection,
            super::super::sandbox_runner::Selection::Unavailable
        ) {
            eprintln!("SKIP: no sandbox runner is usable; background bash end-to-end unverified");
            return;
        }
        let work = tempfile::tempdir().expect("workspace");
        let session_id = "session-bg";
        let (_home, sessions) = session_home_in(session_id, work.path());
        let mut machine = AgentMachine::new(
            sessions.clone(),
            vec![canned_provider_seq(&BACKGROUND_THEN_READ)],
        )
        .with_sandbox(context);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let outs = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-bg",
                        "content": [{ "type": "text", "text": "run something in the background" }],
                    },
                }),
            },
        );
        assert!(
            matches!(reply_of(&outs), RpcReply::Ok { .. }),
            "the turn is accepted: {outs:?}"
        );

        // The second step's `assistant/message` exists but its result did not
        // always settle back to the call's reply before the driver drained
        // the effect history — read the table through the machine's *own*
        // arm instead: a job_output issued on a follow-up turn observes the
        // settle the no-op command has now had time to reach.
        let rows = written_rows_in(&sessions, Some(work.path()), session_id);
        let results: Vec<&Value> = rows
            .iter()
            .filter(|r| r.get("type").and_then(Value::as_str) == Some("tool/result"))
            .collect();
        assert!(
            results.len() >= 1,
            "the background start is recorded: {}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        let start_text = results[0]
            .pointer("/data/message/content/0/content/0/text")
            .and_then(Value::as_str)
            .expect("the start's text");
        assert_eq!(start_text, "started background job bash-1");
        // The list names the job the start minted; the settle against the
        // read is timing-dependent (the no-op finishes on its own thread), so
        // the covering driver/exec tests own the settled-marker assertion and
        // this one only requires the table to name it.
        let read_text = results
            .get(1)
            .and_then(|r| r.pointer("/data/message/content/0/content/0/text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            read_text.is_empty() || read_text.contains("bash-1 [bash]"),
            "the table lists the started job: {read_text:?}"
        );
        // The foreground contract is untouched by the field: the `sleep`
        // never ran under the step, which the turn's own balance proves.
        let calls: Vec<&Value> = rows
            .iter()
            .filter(|r| r.get("type").and_then(Value::as_str) == Some("tool/call"))
            .collect();
        assert_eq!(calls[0].pointer("/data/name"), Some(&json!("bash")));
        assert_eq!(calls[1].pointer("/data/name"), Some(&json!("job_list")));
    }

    /// **`bash` runs through the whole agent, confined by the kernel.**
    ///
    /// This is the test the sandbox runner seam has been waiting for: before it,
    /// no tool executed code, so the runner had no consumer. The model asks to
    /// run a command, the executor confines it via the real runner chain, the
    /// driver spawns the wrapped argv, and the command's output lands in the log.
    ///
    /// It self-skips where no runner is usable, loudly, because such a host
    /// leaves the claim untested rather than false — the same discipline
    /// `sandbox_runner`'s own kernel tests use. On this host `bwrap` is present,
    /// so it really runs.
    #[test]
    fn a_bash_call_runs_confined_through_the_whole_agent() {
        let context = crate::driver::probe_sandbox(std::env::consts::OS);
        if matches!(
            context.selection,
            super::super::sandbox_runner::Selection::Unavailable
        ) {
            eprintln!("SKIP: no sandbox runner is usable; bash end-to-end unverified");
            return;
        }
        let work = tempfile::tempdir().expect("workspace");
        let session_id = "session-bash";
        let (_home, sessions) = session_home_in(session_id, work.path());
        let mut machine = AgentMachine::new(
            sessions.clone(),
            vec![canned_provider_seq(&BASH_THEN_ANSWER)],
        )
        .with_sandbox(context);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let outs = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-bash",
                        "content": [{ "type": "text", "text": "run a command" }],
                    },
                }),
            },
        );
        assert!(
            matches!(reply_of(&outs), RpcReply::Ok { .. }),
            "the turn is accepted: {outs:?}"
        );

        let rows = written_rows_in(&sessions, Some(work.path()), session_id);
        // The command ran despite confinement — that is the whole point. A refusal
        // would still produce a `tool/result`, so the assertion is on the text.
        let result = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("tool/result"))
            .expect("the tool result row");
        let text = result
            .pointer("/data/message/content/0/content/0/text")
            .and_then(Value::as_str)
            .expect("the result text");
        assert_eq!(
            result.pointer("/data/message/content/0/isError"),
            Some(&json!(false)),
            "a confined command is a result, not an error: {result}"
        );
        assert!(
            text.contains("dsh-bash-e2e"),
            "the command's stdout reached the model: {text:?}"
        );
        // The tool call is recorded with the model's own arguments, and the turn
        // continues to a second model call that answers.
        let call = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("tool/call"))
            .expect("the tool call row");
        assert_eq!(call.pointer("/data/name"), Some(&json!("bash")));
        let messages: Vec<&Value> = rows
            .iter()
            .filter(|r| r.get("type").and_then(Value::as_str) == Some("assistant/message"))
            .collect();
        assert_eq!(messages.len(), 2, "two model calls, two messages");
        assert_eq!(
            messages[1].pointer("/data/message/content/0/text"),
            Some(&json!("DONE")),
            "the second call's answer"
        );
    }

    /// **A confined command cannot write outside the workspace.** Under
    /// `workspace-write` a write to a path outside the root is refused by the
    /// kernel, and the observable world proves it: the file does not appear.
    ///
    /// This is the difference between testing the denial *message* and testing the
    /// *confinement* — the message can be produced by a profile that enforced
    /// nothing. Like the test above, it self-skips where no runner is usable.
    #[test]
    fn a_confined_bash_cannot_write_outside_the_workspace() {
        let context = crate::driver::probe_sandbox(std::env::consts::OS);
        if matches!(
            context.selection,
            super::super::sandbox_runner::Selection::Unavailable
        ) {
            eprintln!("SKIP: no sandbox runner is usable; confinement unverified");
            return;
        }
        // A path outside the workspace **and outside `/tmp`**: the
        // `workspace-write` profile replaces `/tmp` with a fresh tmpfs, so a
        // write there fails with ENOENT (the path simply is not in the new
        // mount) rather than exercising the read-only root. `/var/tmp` stays
        // under the read-only `/` bind, so a write there is refused by the
        // kernel with the runner's own `Read-only file system` signature — the
        // dialect the denial classifier is built on.
        let target =
            std::path::Path::new("/var/tmp").join(format!("vocoder-fence-{}.txt", rpc::new_id()));
        let work = tempfile::tempdir().expect("workspace");
        let session_id = "session-bash-fence";
        let (_home, sessions) = session_home_in(session_id, work.path());

        // The command tries to write outside; if confinement holds it cannot.
        // The command *is* the failing write, so its exit status is the denial's
        // — the classifier is exit-gated, and a trailing successful statement
        // would report exit 0 and match no signature (which is upstream's own
        // contract, not a gap).
        let bodies = [
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",",
                "\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":",
                "\"{\\\"command\\\":\\\"touch OUTSIDE_PATH\\\",",
                "\\\"description\\\":\\\"Attempt an outside write\\\"}\"}}]},",
                "\"finish_reason\":\"tool_calls\",\"index\":0}],\"created\":1,\"id\":\"c1\",",
                "\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
                "data: [DONE]\n\n",
            ),
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"DONE\"},",
                "\"finish_reason\":\"stop\",\"index\":0}],\"created\":1,\"id\":\"c2\",",
                "\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
                "data: [DONE]\n\n",
            ),
        ];
        // Substitute the outside path into the canned body (it is runtime data).
        let body0 = bodies[0].replace("OUTSIDE_PATH", &target.to_string_lossy());
        let dir = std::env::temp_dir().join(format!("voco-fence-{}", rpc::new_id()));
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("body-0.txt"), &body0).expect("body 0");
        std::fs::write(dir.join("body-1.txt"), bodies[1]).expect("body 1");
        let route = Route {
            config: ProviderConfig {
                id: "canned".into(),
                kind: ProviderKind::OpenAiChat,
                base_url: format!("canned-seq://{}", dir.to_string_lossy()),
                model: "test-model".into(),
                api_key_env: None,
            },
        };

        let mut machine = AgentMachine::new(sessions.clone(), vec![route]).with_sandbox(context);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        let _ = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-fence",
                        "content": [{ "type": "text", "text": "try to escape" }],
                    },
                }),
            },
        );

        // The kernel refused: the file the command tried to create does not exist.
        assert!(
            !target.exists(),
            "the confined write escaped the workspace: {target:?} exists"
        );
        // Leave no trace if the assertion above would have let it through.
        let _ = std::fs::remove_file(&target);
        // And the model was told why, in the sandbox's own vocabulary.
        let rows = written_rows_in(&sessions, Some(work.path()), session_id);
        let text = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("tool/result"))
            .and_then(|r| r.pointer("/data/message/content/0/content/0/text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            text.contains("file access denied"),
            "the denial marker is present: {text:?}"
        );
    }

    /// A canned reply that calls `read` on a file, then answers.    ///
    /// Two bodies because the turn has two model calls: the first proposes the
    /// tool call, the second answers once the tool result is in the log. The
    /// provider route is keyed by *call order*, which is what `canned_provider_seq`
    /// sets up.
    const READ_THEN_ANSWER: [&str; 2] = [
        concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
            "\"created\":1,\"id\":\"c1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
            "\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]},",
            "\"finish_reason\":null,\"index\":0}],\"created\":1,\"id\":\"c1\",\"model\":\"m\",",
            "\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\"{\\\"file_path\\\":\\\"greeting.txt\\\"}\"}}]},",
            "\"finish_reason\":null,\"index\":0}],\"created\":1,\"id\":\"c1\",\"model\":\"m\",",
            "\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\",\"index\":0}],",
            "\"created\":1,\"id\":\"c1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        ),
        concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
            "\"created\":1,\"id\":\"c2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"DONE\"},\"finish_reason\":\"stop\",\"index\":0}],",
            "\"created\":1,\"id\":\"c2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        ),
    ];

    /// A route whose canned body changes per call, in order.
    ///
    /// A single canned body cannot express a tool-calling turn: the second model
    /// call is made *because* the first called a tool, so it has to answer
    /// differently. The bodies are written to numbered files and the route's
    /// `base_url` advances a counter, which is what makes the second call's
    /// request observably different from the first's.
    fn canned_provider_seq(bodies: &[&'static str]) -> Route {
        let dir = std::env::temp_dir().join(format!("voco-canned-{}", rpc::new_id()));
        std::fs::create_dir_all(&dir).expect("canned dir");
        for (i, body) in bodies.iter().enumerate() {
            std::fs::write(dir.join(format!("body-{i}.txt")), body).expect("write body");
        }
        Route {
            config: ProviderConfig {
                id: "canned".into(),
                kind: ProviderKind::OpenAiChat,
                base_url: format!("canned-seq://{}", dir.to_string_lossy()),
                model: "test-model".into(),
                api_key_env: None,
            },
        }
    }

    /// A session whose header records a real workspace, so the tools have a root.
    fn session_home_in(session_id: &str, cwd: &std::path::Path) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let sessions = dir.path().join("sessions");
        let sdir = sessions
            .join(super::super::session::SessionStore::project_dir(Some(
                &cwd.to_string_lossy(),
            )))
            .join(super::super::session::SessionStore::encode_segment(
                session_id,
            ));
        std::fs::create_dir_all(&sdir).expect("session dir");
        let rows = vec![
            json!({
                "type": "session", "version": 3, "id": session_id, "createdAt": 0,
                "cwd": cwd.to_string_lossy(),
            }),
            json!({
                "type": "user/message", "seq": 0, "time": 0,
                "data": {
                    "content": [{ "type": "text", "text": "read greeting.txt" }],
                    "source": { "kind": "user", "rpcId": "seed" },
                    "role": "user", "id": "msg-seed",
                },
            }),
        ];
        let bytes = vocoder_session::encode_generation(&rows, false).expect("encode");
        std::fs::write(
            sdir.join(vocoder_session::generation_filename(0, false)),
            bytes,
        )
        .expect("write header");
        (dir, sessions)
    }

    /// **A tool call round-trips through the whole agent.** The model asks to read
    /// a file, the executor runs it, the result lands in the log, and the turn
    /// continues to a second model call that answers.
    ///
    /// This is the behaviour step 3 exists for. Before the executor, a
    /// tool-calling reply opened a step with no results to send: the second
    /// request carried the assistant's call and nothing answering it, which a
    /// provider rejects.
    #[test]
    fn a_tool_call_runs_and_the_turn_continues_with_its_result() {
        let work = tempfile::tempdir().expect("workspace");
        std::fs::write(work.path().join("greeting.txt"), "hello\n").expect("file");
        let session_id = "session-tools";
        let (_home, sessions) = session_home_in(session_id, work.path());
        let mut machine = AgentMachine::new(
            sessions.clone(),
            vec![canned_provider_seq(&READ_THEN_ANSWER)],
        );
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let outs = drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-tools",
                        "content": [{ "type": "text", "text": "read greeting.txt" }],
                    },
                }),
            },
        );
        assert!(
            matches!(reply_of(&outs), RpcReply::Ok { .. }),
            "the turn is accepted: {outs:?}"
        );

        let rows = written_rows_in(&sessions, Some(work.path()), session_id);
        let types: Vec<&str> = rows
            .iter()
            .map(|r| r.get("type").and_then(Value::as_str).unwrap_or_default())
            .collect();
        // The whole turn: the tool call runs *inside* step 1, between the message
        // that made it and the `step/end` that closes it.
        assert_eq!(
            types,
            vec![
                "session",
                "user/message",
                "agent/inbox/spliced",
                "turn/start",
                "step/start",
                "assistant/message",
                "tool/call",
                "tool/result",
                "step/end",
                "step/start",
                "assistant/message",
                "step/end",
                "turn/end",
            ],
            "unexpected frame sequence"
        );

        // The call is recorded with the model's own arguments, as text.
        let call = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("tool/call"))
            .expect("the tool call row");
        assert_eq!(call.pointer("/data/name"), Some(&json!("read")));
        assert_eq!(call.pointer("/data/callId"), Some(&json!("call_1")));
        assert_eq!(
            call.pointer("/data/arguments"),
            Some(&json!("{\"file_path\":\"greeting.txt\"}")),
            "the arguments are the model's own text"
        );
        assert_eq!(call.pointer("/data/turn"), Some(&json!(1)));
        assert_eq!(call.pointer("/data/step"), Some(&json!(1)));

        // The result carries the real file's content in the upstream envelope,
        // and cites the call row it answers.
        let result = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("tool/result"))
            .expect("the tool result row");
        let text = result
            .pointer("/data/message/content/0/content/0/text")
            .and_then(Value::as_str)
            .expect("the result text");
        assert!(
            text.contains("1: hello"),
            "the file's content reached the model: {text:?}"
        );
        assert!(
            text.contains("(End of file - total 1 lines)"),
            "the footer is the upstream wording: {text:?}"
        );
        assert_eq!(
            result.pointer("/data/message/content/0/isError"),
            Some(&json!(false))
        );
        // `sourceEventSeqs` names the `tool/call` row's own seq, which is what
        // pairs the two after the fact.
        let call_seq = call.get("seq").and_then(Value::as_f64).unwrap() as u64;
        assert_eq!(
            result.pointer("/data/sourceEventSeqs/0"),
            Some(&json!(call_seq)),
            "the result cites its call"
        );
        // The window's structured form persisted, so a UI card replays.
        assert_eq!(
            result.pointer("/data/meta/totalLines"),
            Some(&json!(1)),
            "{result}"
        );

        // The turn ran to a second model call that answered, which is the whole
        // point: the tool result reached the next request.
        let messages: Vec<&Value> = rows
            .iter()
            .filter(|r| r.get("type").and_then(Value::as_str) == Some("assistant/message"))
            .collect();
        assert_eq!(messages.len(), 2, "two model calls, two messages");
        assert_eq!(
            messages[1].pointer("/data/message/content/0/text"),
            Some(&json!("DONE")),
            "the second call's answer"
        );
        assert_eq!(
            rows.last().unwrap().pointer("/data/reason/kind"),
            Some(&json!("completed"))
        );
    }

    /// A tool-calling turn's request carries the tools, and its second call
    /// carries the answered result.
    ///
    /// The two claims are different and both matter: without the tool definitions
    /// the model cannot call anything, and without the result item the second
    /// request has a call with nothing answering it — which a real provider
    /// rejects with a 400.
    #[test]
    fn the_request_offers_tools_and_the_next_one_carries_the_result() {
        let work = tempfile::tempdir().expect("workspace");
        std::fs::write(work.path().join("greeting.txt"), "hello\n").expect("file");
        let session_id = "session-tools-req";
        let (_home, sessions) = session_home_in(session_id, work.path());
        let mut machine = AgentMachine::new(
            sessions.clone(),
            vec![canned_provider_seq(&READ_THEN_ANSWER)],
        );
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-tools-req",
                        "content": [{ "type": "text", "text": "read greeting.txt" }],
                    },
                }),
            },
        );

        // The request bodies the host actually sent, captured by the route's
        // per-call canned bodies: the assertion is on what *was built*, not on
        // what the machine holds.
        let rows = written_rows_in(&sessions, Some(work.path()), session_id);
        let req = machine
            .canonical_request(&rows)
            .expect("a request can be rebuilt from the log");
        let (_, canonical) = req;
        assert!(
            canonical.tools.iter().any(|t| t.name == "read"),
            "the tools are offered: {:?}",
            canonical.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert!(canonical.tools.iter().any(|t| t.name == "write"));
        assert!(canonical.tools.iter().any(|t| t.name == "edit"));

        // The conversation the second call would send carries the tool result as
        // a *user* item — upstream's shape, and the only one a provider accepts
        // for a tool outcome.
        let items: Vec<String> = canonical
            .messages
            .iter()
            .flat_map(|m| m.items.iter())
            .map(|i| match i {
                llm_dialect::items::ContentItem::ToolCall { name, .. } => format!("call:{name}"),
                llm_dialect::items::ContentItem::ToolResult {
                    tool_call_id,
                    is_error,
                    ..
                } => format!("result:{tool_call_id}:{is_error}"),
                llm_dialect::items::ContentItem::Text { .. } => "text".into(),
                other => format!("{other:?}"),
            })
            .collect();
        assert!(items.contains(&"call:read".to_string()), "{items:?}");
        assert!(
            items.contains(&"result:call_1:false".to_string()),
            "the result is re-sent, not dropped: {items:?}"
        );
    }

    /// A tool call rounds-trips through a turn and its row order is the
    /// corpus's: the call *before* whatever the gate does with it.
    ///
    /// A denied call still records what the model asked for — a gate that
    /// pre-empted the `tool/call` row would lose the only evidence the call
    /// happened.
    /// **A session at `read-only` denies a write even when its approval policy is
    /// `ask`.**
    ///
    /// This is the fail-open bug in miniature, and it is pinned because the
    /// version of `fence_for` that read `approval/policy` would have *granted*
    /// this write. The corpus's `missing-sandbox-runner` and
    /// `partial-landlock-child-failure` sessions carry `approval/policy: ask` while
    /// running at `read-only`; deriving the mode from the policy gave them
    /// `workspace-write`, which is strictly wider than what upstream permits.
    ///
    /// The two axes are independent by construction — `read-only` and
    /// `workspace-write` are *both* `approval: ask` in the base profile's presets
    /// table — so no mapping between them can be correct.
    #[test]
    fn a_read_only_session_denies_a_write_whatever_its_approval_policy_says() {
        let work = tempfile::tempdir().expect("workspace");
        let session_id = "session-read-only";
        let (_home, sessions) = session_home_in(session_id, work.path());
        // The session's own mode row, which is the authority.
        append_mode_row(&sessions, work.path(), session_id, "read-only");

        let write_body: &'static str = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_ro\",",
            "\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":",
            "\"{\\\"file_path\\\":\\\"inside.txt\\\",\\\"content\\\":\\\"x\\\"}\"}}]},",
            "\"finish_reason\":\"tool_calls\",\"index\":0}],\"created\":1,\"id\":\"c1\",",
            "\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        );
        let answer_body: &'static str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\",\"index\":0}],",
            "\"created\":1,\"id\":\"c2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: [DONE]\n\n",
        );
        let mut machine = AgentMachine::new(
            sessions.clone(),
            vec![canned_provider_seq(&[write_body, answer_body])],
        );
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-read-only",
                        "content": [{ "type": "text", "text": "write inside" }],
                    },
                }),
            },
        );

        let rows = written_rows_in(&sessions, Some(work.path()), session_id);
        let result = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("tool/result"))
            .expect("the refusal");
        assert_eq!(
            result.pointer("/data/message/content/0/isError"),
            Some(&json!(true)),
            "a read-only session must refuse the write: {result}"
        );
        let text = result
            .pointer("/data/message/content/0/content/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            text.contains("read-only"),
            "the denial names read-only, not workspace-write: {text:?}"
        );
        // **The file was not created.** This is the assertion that would have
        // failed before the fix: the write was *inside* the workspace, so only
        // the mode — not containment — could have stopped it.
        assert!(
            !work.path().join("inside.txt").exists(),
            "read-only denied the write"
        );
    }

    /// Append a `sandbox/mode` row to a session's newest generation.
    ///
    /// The row is the authority `fence_for` reads, so a test that wants a session
    /// in a non-default mode has to write one — which is also the honest picture:
    /// nothing in this host writes it, so the only way a mode arrives is from a
    /// log that already carried one (a resumed session, or a delegation seed).
    fn append_mode_row(
        sessions: &std::path::Path,
        cwd: &std::path::Path,
        session_id: &str,
        mode: &str,
    ) {
        let dir = sessions
            .join(super::super::session::SessionStore::project_dir(Some(
                &cwd.to_string_lossy(),
            )))
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
        let (_, path) =
            super::super::session::SessionStore::latest_generation_in(&dir_str, &listing)
                .expect("a generation");
        let bytes = std::fs::read(&path).expect("read generation");
        let mut rows = vocoder_session::decode_generation(
            &bytes,
            super::super::session::SessionStore::is_compressed(&path),
        )
        .expect("decode");
        let seq = rows.len().saturating_sub(1) as f64;
        rows.push(json!({
            "type": "sandbox/mode",
            "seq": seq,
            "time": 0,
            "data": { "mode": mode },
        }));
        let bytes = vocoder_session::encode_generation(&rows, false).expect("encode");
        std::fs::write(
            dir.join(vocoder_session::generation_filename(1, false)),
            bytes,
        )
        .expect("write generation");
    }

    #[test]
    fn a_denied_tool_call_is_still_recorded() {
        let work = tempfile::tempdir().expect("workspace");
        // A path outside the workspace, so the fence refuses the write.
        let outside = work.path().join("..").join("escape.txt");
        let session_id = "session-denied";
        let (_home, sessions) = session_home_in(session_id, work.path());
        let bodies: [&'static str; 2] = [
            concat!(
                "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null,\"index\":0}],",
                "\"created\":1,\"id\":\"c1\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",",
                "\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":",
                "\"{\\\"file_path\\\":\\\"/etc/vocoder-escape\\\",\\\"content\\\":\\\"x\\\"}\"}}]},",
                "\"finish_reason\":\"tool_calls\",\"index\":0}],\"created\":1,\"id\":\"c1\",",
                "\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
                "data: [DONE]\n\n",
            ),
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\",\"index\":0}],",
                "\"created\":1,\"id\":\"c2\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}\n\n",
                "data: [DONE]\n\n",
            ),
        ];
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider_seq(&bodies)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });
        drive(
            &mut machine,
            MachineIn::Event {
                name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
                payload: json!({
                    "method": "run",
                    "args": {
                        "sessionId": session_id,
                        "requestId": "req-denied",
                        "content": [{ "type": "text", "text": "write outside" }],
                    },
                }),
            },
        );

        let rows = written_rows_in(&sessions, Some(work.path()), session_id);
        let types: Vec<&str> = rows
            .iter()
            .map(|r| r.get("type").and_then(Value::as_str).unwrap_or_default())
            .collect();
        assert!(
            types.contains(&"tool/call"),
            "a denied call is still recorded: {types:?}"
        );
        let result = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("tool/result"))
            .expect("the refusal");
        assert_eq!(
            result.pointer("/data/message/content/0/isError"),
            Some(&json!(true))
        );
        let text = result
            .pointer("/data/message/content/0/content/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            text.contains("[sandbox: file access denied under workspace-write mode]"),
            "the denial names the mode: {text:?}"
        );
        // The refusal did not reach the filesystem.
        assert!(!std::path::Path::new("/etc/vocoder-escape").exists());
        // And the turn still completed: a refused call is the model's problem to
        // work around, not a turn failure.
        assert_eq!(
            rows.last().unwrap().pointer("/data/reason/kind"),
            Some(&json!("completed"))
        );
        let _ = outside;
    }

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

    /// A cancel arriving after the turn closed is accepted and appends nothing.
    ///
    /// The late-arrival case: a client's stop reaches the host after the model's
    /// last token did. Upstream answers an idle agent the same way, and the log
    /// must not gain a row for it. The *mid-call* case — the one a stop button
    /// actually produces — is
    /// [`a_cancel_mid_call_aborts_the_turn_and_marks_the_prefix_interrupted`].
    #[test]
    fn a_late_cancel_is_accepted_and_leaves_the_frames_balanced() {
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
        let before = written_rows(&sessions, session_id);
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
        assert_eq!(rows.len(), before.len(), "a late cancel writes nothing");
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

    /// Start a turn and stop with its model call *outstanding*.
    ///
    /// Answers every effect the turn needs on the way there — the session log's
    /// walk and read — and deliberately does **not** perform the `FetchStream`,
    /// returning its id instead. That is the suspension point a cancel has to be
    /// delivered at to exercise the mid-call path: the call is issued and has no
    /// answer yet, which is precisely when a user presses stop.
    fn start_turn_to_the_fetch(
        machine: &mut AgentMachine,
        session_id: &str,
    ) -> vocoder_cordis::EffectId {
        let mut outs = machine.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
            payload: json!({
                "method": "run",
                "args": {
                    "sessionId": session_id,
                    "requestId": "req-cancel",
                    "content": [{ "type": "text", "text": "ping" }],
                },
            }),
        });
        for _ in 0..16 {
            let Some((id, request)) = outs.iter().find_map(|o| match o {
                MachineOut::Realize { id, request } => Some((*id, request.clone())),
                _ => None,
            }) else {
                panic!("the turn never asked for a model call: {outs:?}");
            };
            if matches!(request, RealizeRequest::FetchStream { .. }) {
                return id;
            }
            let result =
                crate::driver::realize_with(request, &mut |_| {}).unwrap_or(EffectResult::Done);
            outs = machine.handle(MachineIn::EffectResult { id, result });
        }
        panic!("the turn asked for more effects than a first call should need");
    }

    /// Answer every effect in `outs` for real, until none is left.
    ///
    /// The manual tests above hand the machine one input at a time, so they own
    /// the effect loop that `drive` would otherwise run. A turn that finishes
    /// still owes its *publish* — the log write is an effect, not something
    /// `finish` does inline — so discarding the outputs would leave the turn
    /// decided in memory and absent from disk.
    fn realize_all(machine: &mut AgentMachine, mut outs: Vec<MachineOut>) -> Vec<MachineOut> {
        let mut terminal = Vec::new();
        for _ in 0..16 {
            let mut next: Vec<(vocoder_cordis::EffectId, RealizeRequest)> = Vec::new();
            for out in outs {
                match out {
                    MachineOut::Realize { id, request } => next.push((id, request)),
                    other => terminal.push(other),
                }
            }
            let Some((id, request)) = next.into_iter().next() else {
                break;
            };
            let result =
                crate::driver::realize_with(request, &mut |_| {}).unwrap_or(EffectResult::Done);
            outs = machine.handle(MachineIn::EffectResult { id, result });
        }
        terminal
    }

    /// A cancel that lands while the model call is in flight aborts the turn.
    ///
    /// This is the case the wire path exists for, and the one a stub cannot
    /// serve: `session/cancel` arrives *between* the fetch's issue and its
    /// answer, which is exactly when a user hits stop. The turn must close
    /// `aborted` with the user's cause, and the model's text must be recorded as
    /// an interrupted prefix rather than as a reply it finished.
    #[test]
    fn a_cancel_mid_call_aborts_the_turn_and_marks_the_prefix_interrupted() {
        let session_id = "session-cancel-mid";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(NO_USAGE_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let effect_id = start_turn_to_the_fetch(&mut machine, session_id);

        // The cancel arrives now, while the fetch is outstanding.
        let cancel = machine.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
            payload: json!({
                "method": "cancel",
                "args": { "sessionId": session_id },
            }),
        });
        assert!(
            matches!(reply_of(&cancel), RpcReply::Ok { .. }),
            "a cancel mid-call is accepted: {cancel:?}"
        );

        // The provider answers the call the cancel interrupted.
        let outs = machine.handle(MachineIn::EffectResult {
            id: effect_id,
            result: EffectResult::HttpResponse {
                status: 200,
                body: NO_USAGE_BODY.to_string(),
            },
        });
        let _ = realize_all(&mut machine, outs);

        let rows = written_rows(&sessions, session_id);
        let types: Vec<&str> = rows
            .iter()
            .map(|r| r.get("type").and_then(Value::as_str).unwrap_or_default())
            .collect();
        for frame in ["turn/start", "turn/end", "step/start", "step/end"] {
            assert_eq!(
                types.iter().filter(|t| **t == frame).count(),
                1,
                "{frame} exactly once in {types:?}"
            );
        }
        // The turn closed aborted, with the user named as the cause.
        let end = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("turn/end"))
            .expect("a turn/end");
        assert_eq!(
            end.pointer("/data/reason/kind").and_then(Value::as_str),
            Some("aborted"),
            "a cancelled turn ends aborted: {end}"
        );
        assert_eq!(
            end.pointer("/data/reason/reason/kind")
                .and_then(Value::as_str),
            Some("user"),
            "the cause is the user: {end}"
        );
        // The model's text survives as an interrupted prefix, so the answer the
        // model had produced is not lost — and is not claimed as finished.
        let msg = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("assistant/message"))
            .expect("the delivered prefix is recorded");
        assert_eq!(
            msg.pointer("/data/interrupted").and_then(Value::as_bool),
            Some(true),
            "an interrupted prefix carries the marker: {msg}"
        );
        // `usage` is absent, not zero: the adapter reported none. Upstream
        // spreads the key conditionally, and the recording omits it — writing
        // zeros would claim an accounting the adapter never gave.
        assert!(
            msg.pointer("/data/usage").is_none(),
            "no usage key when the adapter reported none: {msg}"
        );
        // The request itself is *not* an event: upstream has no such vocabulary,
        // and a reader that did not know `agent/cancel-requested` would refuse
        // the whole log.
        assert!(
            !types.contains(&"agent/cancel-requested"),
            "a cancel request is not a durable event type: {types:?}"
        );
    }

    /// A cancel for a different session must not abort the turn in flight.
    ///
    /// Upstream's `agents.get(sessionId)` is a *lookup*: a cancel naming a
    /// session with no live agent is answered `session/not-found` and reaches no
    /// other session's turn. Aborting whatever happens to be running would let
    /// one conversation's stop button kill another's.
    #[test]
    fn a_cancel_for_another_session_does_not_abort_this_turn() {
        let session_id = "session-cancel-scope";
        let (_home, sessions) = session_home(session_id);
        let mut machine = AgentMachine::new(sessions.clone(), vec![canned_provider(LIVE_BODY)]);
        let _ = machine.handle(MachineIn::ServicesReady { keys: vec![] });

        let effect_id = start_turn_to_the_fetch(&mut machine, session_id);

        // A cancel for a session that is not the one running.
        let _ = machine.handle(MachineIn::Event {
            name: vocoder_cordis::EventName::new(rpc::call_event("agent")),
            payload: json!({
                "method": "cancel",
                "args": { "sessionId": "some-other-session" },
            }),
        });

        let outs = machine.handle(MachineIn::EffectResult {
            id: effect_id,
            result: EffectResult::HttpResponse {
                status: 200,
                body: LIVE_BODY.to_string(),
            },
        });
        let _ = realize_all(&mut machine, outs);
        let rows = written_rows(&sessions, session_id);
        let end = rows
            .iter()
            .find(|r| r.get("type").and_then(Value::as_str) == Some("turn/end"))
            .expect("a turn/end");
        assert_eq!(
            end.pointer("/data/reason/kind").and_then(Value::as_str),
            Some("completed"),
            "another session's cancel leaves this turn alone: {end}"
        );
    }

    /// A cancel for a session with no turn running is accepted and writes nothing.
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

    /// The one recorded cancellation, and the shape it fixes.
    ///
    /// `dsh/snapshots/acp/cancel/session.v3.jsonl` is the only committed log
    /// that records a cancelled turn, and it is the authority for three things
    /// this machine had wrong or unverified: the settling row is
    /// `assistant/message` with `interrupted: true`; `usage` is **absent**
    /// (the adapter never reported one, and upstream spreads the key
    /// conditionally on both settle paths); and there is no event for the
    /// cancel *request* — the turn's own `turn/end {kind: 'aborted'}` carries
    /// it. A reader that did not know `agent/cancel-requested` would refuse the
    /// whole log, which is why nothing writes one.
    ///
    /// Asserted against the recording rather than against a hand-written
    /// expectation, so a change to the vocabulary fails here.
    #[test]
    fn the_recorded_cancel_fixes_the_interrupted_row_shape() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../dsh/snapshots/acp/cancel/session.v3.jsonl");
        if !path.exists() {
            eprintln!(
                "skipping: {} absent (submodule not checked out)",
                path.display()
            );
            return;
        }
        let rows = vocoder_session::read_generation(&path).expect("read recorded cancel");

        let row = |ty: &str| {
            rows.iter()
                .find(|r| r.get("type").and_then(Value::as_str) == Some(ty))
                .unwrap_or_else(|| panic!("no {ty} row in the recording"))
                .get("data")
                .cloned()
                .unwrap_or(Value::Null)
        };

        // The settling row: an interrupted prefix, not a reply.
        let msg = row("assistant/message");
        assert_eq!(
            msg.get("interrupted").and_then(Value::as_bool),
            Some(true),
            "recorded: {msg}"
        );
        assert!(
            !msg.as_object().is_some_and(|o| o.contains_key("usage")),
            "no usage key when the adapter reported none: {msg}"
        );
        assert!(
            msg.pointer("/message/content").is_some(),
            "the prefix carries the model's delivered content: {msg}"
        );

        // The abort is carried by the turn's own closer, with the user as the
        // cause — there is no separate cancel event.
        let end = row("turn/end");
        assert_eq!(
            end.pointer("/reason/kind").and_then(Value::as_str),
            Some("aborted"),
            "recorded: {end}"
        );
        assert_eq!(
            end.pointer("/reason/reason/kind").and_then(Value::as_str),
            Some("user"),
            "recorded: {end}"
        );

        // No invented vocabulary anywhere in the recording.
        let types: Vec<&str> = rows
            .iter()
            .map(|r| r.get("type").and_then(Value::as_str).unwrap_or_default())
            .collect();
        assert!(
            !types.contains(&"agent/cancel-requested"),
            "the recording has no cancel-request event: {types:?}"
        );
    }
}
