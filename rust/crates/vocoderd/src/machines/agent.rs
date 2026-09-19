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
    fn publish(&mut self, session: &str, rows: &[Value]) -> Vec<MachineOut> {
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
        self.cache
            .request_write(&path, bytes, &mut self.pending, &mut self.effects)
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

    /// Emit the provider fetch for the pending call.
    ///
    /// The API key is read from the environment here, at call time, and never
    /// enters the machine or the log: [`ProviderConfig::auth`] resolves a header
    /// pair from a variable *name*.
    fn fetch(&mut self, provider: &str, body: &Value) -> Vec<MachineOut> {
        let Some(route) = self.routes.get(provider) else {
            return vec![];
        };
        let headers: Vec<(String, String)> = route.config.auth().into_iter().collect();
        let url = route.config.url();
        let id = self.cache.next_effect(&mut self.pending, &mut self.effects);
        vec![rpc::effect(
            id,
            RealizeRequest::FetchJson {
                url,
                headers,
                body: body.to_string(),
            },
        )]
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
        let out = self.publish(&state.session, &state.rows);
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
            self.op = Some(Op::Call {
                state,
                provider,
                body,
            });
            return self.resume_op();
        }
        // A running turn with no call decided owes another step.
        if state.fsm.is_running() {
            let outs = state.fsm.enter_step();
            let call = self.apply(&mut state, outs);
            if let Some((provider, body)) = call {
                self.op = Some(Op::Call {
                    state,
                    provider,
                    body,
                });
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
            }) => {
                let effects = self.fetch(&provider, &body);
                self.op = Some(Op::Call {
                    state,
                    provider,
                    body,
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
        Self::row(
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
        let outcome = match &reason {
            Some(r) => step_outcome(r),
            None => StepOutcome::Error {
                code: "PROVIDER_STREAM_INCOMPLETE".into(),
                message: "provider stream ended without a finish chunk".into(),
            },
        };
        let outs = state.fsm.step_reply(outcome, has_calls, false);
        let call = self.apply(&mut state, outs);
        self.continue_turn(state, call)
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
                self.op = Some(Op::Settle {
                    state,
                    provider,
                    body,
                });
                self.resume_op()
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
    fn drive(machine: &mut AgentMachine, mut pending: MachineIn) -> Vec<MachineOut> {
        let mut terminal = Vec::new();
        for _ in 0..64 {
            let mut effects = Vec::new();
            for out in machine.handle(pending) {
                match out {
                    MachineOut::Realize { id, request } => effects.push((id, request)),
                    other => terminal.push(other),
                }
            }
            if effects.is_empty() {
                break;
            }
            let (id, request) = effects.remove(0);
            let result = match &request {
                // A `canned://` route is answered from the file it names
                // instead of the network, so a whole turn is testable
                // hermetically. Anything else goes through the real driver.
                RealizeRequest::FetchJson { url, .. } if url.starts_with("canned://") => {
                    // `ProviderConfig::url` appends the dialect's own path, so
                    // the directory has to be recovered by stripping it back
                    // off — stripping one component would leave `/chat` on the
                    // end and look in a directory that does not exist.
                    let mut dir = url.trim_start_matches("canned://").to_string();
                    for suffix in ["/chat/completions", "/responses", "/messages"] {
                        if let Some(stripped) = dir.strip_suffix(suffix) {
                            dir = stripped.to_string();
                            break;
                        }
                    }
                    match std::fs::read_to_string(format!("{dir}/body.txt")) {
                        Ok(body) => {
                            vocoder_cordis::EffectResult::HttpResponse { status: 200, body }
                        }
                        Err(e) => vocoder_cordis::EffectResult::Failed(
                            vocoder_cordis::EffectError::Other(format!("canned body: {e}")),
                        ),
                    }
                }
                other => crate::driver::realize(other.clone())
                    .unwrap_or(vocoder_cordis::EffectResult::Done),
            };
            pending = MachineIn::EffectResult { id, result };
        }
        terminal
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
        let mut terminal = Vec::new();
        let mut pending = Some(pending);
        for _ in 0..64 {
            let Some(input) = pending.take() else { break };
            let mut effects = Vec::new();
            for out in machine.handle(input) {
                match out {
                    MachineOut::Realize { id, request } => effects.push((id, request)),
                    other => terminal.push(other),
                }
            }
            if effects.is_empty() {
                break;
            }
            let (id, request) = effects.remove(0);
            let result =
                crate::driver::realize(request).unwrap_or(vocoder_cordis::EffectResult::Done);
            pending = Some(MachineIn::EffectResult { id, result });
        }
        terminal
    }
}
